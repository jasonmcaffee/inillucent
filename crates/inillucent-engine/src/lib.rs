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
// **Re-exported at the root because that is where they were (task-1962, A1
// step 1).** The split moved these two modules' free functions out of `lib.rs`;
// every call site in this crate names them unqualified, and a move that changes
// no behaviour has no business rewriting all of them.
// The four `inillucent-migrate` names, which were `pub` at this root before the
// split and stay `pub` at it after.
pub(crate) use engine::explain::*;
pub(crate) use engine::rowshape::*;
pub use engine::rowshape::{identity_columns, logical_row, source_layout_of, stored_as};
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
mod rebuild;
mod recovery;
use recovery::{open_file, OpenedFile};
/// Session-scoped `total_changes()` accounting.
mod session_changes;
/// The engine's half of a vector index a module owns.
mod vectors;
pub mod vtab;

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
use inillucent_sql::bind::{AllowAll, Binder, BoundStatement};
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
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::types::ColumnSpec;
use inillucent_tree::write::TreeLog;
use inillucent_tree::PagedTree;
use inillucent_value::collation::Collation;
pub use inillucent_vfs as vfs;
use inillucent_vfs::{DbPath, OsVfs};
use inillucent_wal::{Body, Synchronous, Wal, WalOptions, FIRST_LSN};

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
/// Re-exported for the reason [`BoundParams`] is. It is named `Value` here
/// because `Datum` is already the borrowed form in this crate's own imports.
pub use inillucent_tree::datum::OwnedDatum as Value;

/// How many frames the harness gives a pool when nothing says otherwise.
///
/// 4,096 frames at 32 KiB is 128 MiB, which holds every scorecard fixture at
/// every scale with room to spare - so the default is "resident", and a
/// measurement that wants to compare cache-limited behaviour sets it down
/// explicitly and says so.
pub const DEFAULT_FRAMES: usize = 4_096;

/// A fixture imported into the new engine's trees, in a real database file.
pub struct ImportedDatabase {
    catalog: StaticCatalog,
    database: Database,
    trees: HashMap<u32, PagedTree>,
    layouts: HashMap<u32, std::rc::Rc<SourceLayout>>,
    /// For each table root, its index roots ordered smallest tree first.
    covering: HashMap<u32, Vec<u32>>,
    page_size: usize,
    frames: usize,
    /// The file the trees were written to, kept so it can be reported and
    /// cleaned up.
    path: PathBuf,
    /// The tables the import could not take, by name.
    skipped: Vec<String>,
    limits: Limits,
    /// The write-ahead log every change is described in before it happens.
    ///
    /// Held beside the file rather than inside an `inillucent-txn` `Engine`,
    /// because the read path takes `&Pool` as a plain borrow and an engine
    /// keeps its file behind a `RefCell` that cannot lend one. What this
    /// harness needs of a transaction manager is the log, the sync policy and
    /// the commit record; the snapshots and the version log are what the Phase
    /// 3 model driver exercises, and it drives the `Engine` directly.
    wal: std::rc::Rc<Wal>,
    /// The transaction number the next statement takes.
    next_txn: std::cell::Cell<u64>,
    /// The transaction the statement in flight took, outside a batch.
    ///
    /// **Because `next_txn` is not the number of the statement that is
    /// running.** [`ImportedDatabase::write`] reads `next_txn` and moves it on
    /// in the same breath, so for the rest of that statement `next_txn` names
    /// the *following* transaction - and anything the statement reaches that
    /// asks `current_txn()` is told that number. `follow_vector_indexes` is
    /// such a caller: it runs after the trees are no longer borrowed, so that
    /// it can undo, and it reaches `change_module`, which builds its log with
    /// `current_txn()`. The table's row was logged under the statement's
    /// transaction and the index entry under the next one, which nothing ever
    /// commits - so every insert into a table carrying a vector index lost its
    /// index entry, and a `CREATE INDEX` over a full table lost the whole
    /// backfill. Measured before the fix: five inserts through five statements
    /// left `t_v_state` reading `rows 0`.
    ///
    /// `undo_to_floor` worked around the same hazard by taking the number as a
    /// parameter; this holds it once so that every caller is right rather than
    /// the ones somebody remembered.
    ///
    /// `None` inside a batch and between statements, where `next_txn` is the
    /// correct answer.
    statement_txn: std::cell::Cell<Option<u64>>,
    /// The statements already parsed, bound, planned and prepared, by SQL text.
    ///
    /// **Both arms must reuse what they prepared.** SQLite steps a VDBE program
    /// compiled once; a write path that parsed, bound, planned and prepared on
    /// every execution is not measuring the same thing, and the first run of the
    /// write gate said so plainly - `txn.large` at **0.02x**, forty updates in
    /// 1.7 ms against SQLite's 32 us. The work was the compilation, not the
    /// write.
    ///
    /// Keyed by the statement text, which is what a caller re-issues. Behind an
    /// `Rc` so an entry can be held across the `&mut self` a write needs.
    statements: std::cell::RefCell<HashMap<u64, HashMap<String, std::rc::Rc<Cached>>>>,
    /// The plan cache's ceiling; see `plans.rs`, which holds and enforces it.
    statement_cache_limit: std::cell::Cell<usize>,
    /// Whether the connected modules have been told this transaction started.
    ///
    /// `begin` fires once per write transaction that reaches a module, and this
    /// is what makes "once" true: `change_module` reads it before the first
    /// write and the commit and the rollback clear it. See
    /// `vtab::begin_modules`.
    modules_begun: std::cell::Cell<bool>,
    /// How many statements this connection has actually compiled.
    ///
    /// **The counter a plan-cache guard needs, and the reason it is a counter.**
    /// "Preparing the same statement again is answered from the cache" used to
    /// be asserted as a ratio between two stopwatch readings, and a stopwatch
    /// reading is decided by whatever else the machine is running: the same
    /// assertion read 1.08x on an idle box and 6.6x on a loaded one, and failed
    /// there while announcing that the plan cache was not being consulted. It
    /// was. This number is 1 in both cases, because it counts the compilations
    /// rather than timing them.
    ///
    /// It counts every call to [`ImportedDatabase::compile`], so a prepare
    /// answered from the cache leaves it alone and a prepare that recompiled
    /// moves it. A lever change, a new authorizer or a registration empties the
    /// cache, and the compiles that follow are counted, because they were paid.
    compiles: std::cell::Cell<u64>,
    /// The transaction every statement joins, when one has been opened.
    ///
    /// `None` is autocommit: each statement is its own transaction and pays for
    /// its own commit. That is the right default and it is also the *expensive*
    /// one, which is why the difference has to be expressible - the gate's
    /// `transaction` family is exactly the question of what a commit costs, and
    /// a harness that could only run one grouping could not ask it.
    batch: std::cell::Cell<Option<u64>>,
    /// What the open transaction changed, newest last, so it can be abandoned.
    ///
    /// Empty outside a transaction, and never filled there: an autocommit
    /// statement cannot be rolled back, so it records nothing.
    ///
    /// **Behind a cell because a statement now has several logs.** One write can
    /// touch `main` and a `TEMP` table in the same breath, each through its own
    /// log, and all of them append here - so they hold it shared and take the
    /// cell when they have something to record, rather than one of them holding
    /// it mutably and the others going without.
    undo: std::cell::RefCell<Vec<Before>>,
    /// How many schemas the last commit was decided over.
    ///
    /// **The instrument for the one claim about this protocol that is otherwise
    /// invisible**: that a transaction which wrote one file does not pay for a
    /// super-journal. The files a two-file commit writes are deleted by the
    /// commit itself, so a directory listing afterwards cannot tell the two
    /// paths apart - and the first version of `seal` did take the two-file path
    /// for a one-file insert, silently. On the harness's own side, like
    /// `index_stages`, and nothing in the engine reads it.
    decided_over: std::cell::Cell<usize>,
    /// Which schemas the open transaction has written, one bit per schema.
    ///
    /// **The participant set a cross-file commit is decided over.** A
    /// transaction that wrote one file commits by appending one record, as it
    /// always has; one that wrote two is decided by a super-journal, and this is
    /// what says which it is. Cleared at every commit and every rollback.
    ///
    /// A mask rather than a set, because it is written on **every** statement
    /// and a `BTreeSet` allocates a node the first time each one is inserted
    /// into. Twelve bits is `main`, `temp` and the ten databases
    /// [`MAX_ATTACHED`] allows, which is every schema a connection can hold.
    touched: u16,
    /// Named savepoints, and where each one sits in `undo`.
    marks: Vec<(Vec<u8>, usize)>,
    /// Whether the open transaction was started by `SAVEPOINT` rather than by
    /// `BEGIN`.
    ///
    /// **What tells `release` whether to commit.** SQLite's rule for
    /// `RELEASE` is not "commit whenever the last savepoint goes away" - a
    /// `SAVEPOINT s` inside an explicit `BEGIN` also empties `marks` when `s`
    /// is released, and that must stay open for the `COMMIT` that follows.
    /// It is "commit when the transaction the savepoint stack itself opened
    /// has no savepoints left in it": `SAVEPOINT` outside a transaction opens
    /// one - see `Directive::Savepoint` - and this is set there, at the
    /// instant it does, and cleared wherever the transaction ends
    /// (`commit_batch`, `rollback`).
    implicit_transaction: std::cell::Cell<bool>,
    /// The rowid the last `INSERT` assigned, for `last_insert_rowid`.
    ///
    /// **Deliberately not restored by a rollback.** SQLite documents the value
    /// as the last rowid *attempted*, and `faults.rs` pins that: an insert that
    /// is rolled back still moves it. Restoring it would be a different answer
    /// wearing the same name.
    last_rowid: std::cell::Cell<i64>,
    /// Every row every statement on this database has changed.
    ///
    /// A trigger's rows and a foreign key's cascade are in it, which is
    /// SQLite's rule and is the difference between this and `last_changes`.
    /// Never decremented: a `ROLLBACK` does not put it back, which was measured
    /// against the pinned shell rather than assumed.
    changed_ever: std::cell::Cell<i64>,
    /// What `changed_ever` read when each connection's session was opened, so
    /// `total_changes()` answers for this session alone rather than for every
    /// session this database has ever handed out. See
    /// [`session_changes::SessionChanges`].
    session_change_baseline: session_changes::SessionChanges,
    /// How many rows the most recent write changed, for `changes()`.
    ///
    /// The statement's own rows only - a trigger body's are not in it. A
    /// statement that changed nothing sets it to zero; a `SELECT`, a DDL and a
    /// transaction statement leave it alone.
    last_changes: std::cell::Cell<i64>,
    /// The random built-ins' stream, advanced once per statement.
    seed: std::cell::Cell<u64>,

    /// The catalog tree's rows, with what each one needs beside it.
    ///
    /// Held beside the tree rather than read back out of it on every DDL
    /// statement. The tree is the authority - it is what the file describes
    /// itself with, and `import_with` compares the two after the checkpoint -
    /// but a `DROP` has to find a row by name and the tree is keyed by rowid,
    /// so the alternative is a full scan per statement.
    entries: Vec<Recorded>,
    /// The tables the binder resolves names against, `sqlite_schema` excepted.
    ///
    /// **Derived from `entries`, always**, by `rebuild_tables`. Nothing adds a
    /// table here directly: a schema is one thing, and deriving it twice - once
    /// when a statement runs and once when the catalog is read back - is how the
    /// two come to disagree.
    tables: Vec<TableInfo>,
    /// `sqlite_schema`'s own declaration, re-registered on every rebuild.
    schema_info: TableInfo,
    /// The identifier the next tree a DDL statement creates is registered under.
    ///
    /// Roots here are *identifiers*, not page numbers - the physical root is in
    /// the catalog row - and the imported ones are the fixture's SQLite page
    /// numbers, which start at 1 and count pages. So a DDL-created tree takes a
    /// number from the top half of the range, where no imported table can be,
    /// and `sqlite_schema` keeps `u32::MAX`.
    next_root: u32,
    /// How long a writer waits for the writer slot, in milliseconds.
    ///
    /// `PRAGMA busy_timeout` reads and writes it. The value is carried here
    /// rather than in `inillucent-txn` because this harness holds the log
    /// directly and never takes the writer slot - so what it can honestly do
    /// with the setting is remember it and report it, which is what the pragma
    /// is asked for far more often than it is relied on.
    busy_timeout_ms: u64,
    /// Whether `PRAGMA foreign_keys` is on.
    foreign_keys: bool,
    /// Whether `PRAGMA defer_foreign_keys` has put every immediate check off
    /// until the commit, for the transaction now open.
    defer_foreign_keys: bool,
    /// How many statements are running, for the file lock.
    ///
    /// A statement runs statements - a trigger body, a foreign-key sweep, a
    /// `CHECK` - so the lock is taken on the way into the outermost one and
    /// released on the way out of it. A counter rather than a flag because the
    /// nesting is real and an inner release would drop the file while the outer
    /// statement was still reading it.
    running: usize,
    /// Whether the file lock is held between transactions.
    ///
    /// **`normal` is a real setting now, and the reason it can be reported
    /// honestly.** Before that, this engine took no file lock at all and
    /// reported `exclusive`, which was the closest true description of "nobody
    /// else may touch this". Under `normal` the lock is taken for each
    /// transaction and released after it, so a second process may have the file
    /// in between - which is what the word means. `exclusive` keeps it, which
    /// is faster and is what a single-process application wants.
    locking_exclusive: bool,
    /// How the pre-commit state is protected, which `PRAGMA journal_mode` sets.
    ///
    /// The write-ahead log by default, because it is the faster of the two -
    /// one sync per commit against two. A rollback journal is what an
    /// application selects when it wants the database to be one file after a
    /// clean close, which is the reason a rollback journal is supported at all.
    journal_mode: inillucent_pool::journal::JournalMode,
    /// Whether `PRAGMA ignore_check_constraints` has turned `CHECK` off.
    ///
    /// Like `foreign_keys` it is read by the *binder*, so changing it throws
    /// away the compiled statements: a plan built while checks were on carries
    /// them and would keep carrying them after the pragma turned them off.
    ignore_check_constraints: bool,
    /// What `PRAGMA secure_delete` is set to: 0 off, 1 on, 2 fast.
    ///
    /// On, the bytes a deleted row occupied are overwritten before the space is
    /// reused, so a row that has been deleted is not still readable in the file
    /// by anyone who opens it with a hex editor. Off is SQLite default and
    /// this engine default, because the overwrite is a write.
    secure_delete: u8,
    /// What `PRAGMA auto_vacuum` is set to: 0 none, 1 full, 2 incremental.
    ///
    /// Settable only while the database holds no table, which is SQLite rule -
    /// the mode decides how the file is laid out, and changing it afterwards is
    /// what `VACUUM` is for.
    auto_vacuum: u8,
    /// Whether `PRAGMA automatic_index` lets the planner build one.
    ///
    /// On by default, as in SQLite: an unindexed table on the inner side of a
    /// join is scanned once per outer row, and building a transient index over
    /// it first is cheaper as soon as the outer side has more than a handful of
    /// rows.
    automatic_index: bool,
    /// Whether a cyclic-key sweep is already running.
    ///
    /// The sweep runs statements, and a statement runs the sweep; without this
    /// the first cascade would recur until the stack ran out. It is a flag
    /// rather than a depth because there is exactly one sweep at a time by
    /// construction: it runs after a statement, at the outermost level.
    settling: std::cell::Cell<bool>,
    /// One parse arena, kept and cleared rather than made per statement.
    ///
    /// **Because a statement's parse is mostly trips to the allocator.** Every
    /// vector in an `Ast` is empty at construction and grows on its first push,
    /// so `SELECT 1` took about half a dozen of them - 270 ns of a 1,337 ns
    /// prepare - to build an arena that is thrown away a microsecond later. A
    /// parser handed a cleared arena pushes into capacity that is already there.
    ///
    /// It is taken out on the way in and put back on the way out, so a nested
    /// compile - a trigger body, a foreign-key check - finds the cell empty and
    /// makes its own rather than sharing the outer statement's. Sharing it
    /// would be the inner parse clearing the arena the outer statement is still
    /// holding nodes in.
    scratch_ast: std::cell::RefCell<Option<inillucent_sql::ast::Ast>>,
    /// Which planner optimizations are on.
    ///
    /// Per connection rather than per statement, because a lever is a question
    /// about the *planner* - "is the answer the same with this off" - and a
    /// measurement that varied it per statement would be comparing two plans of
    /// two different queries.
    levers: Levers,
    /// What `PRAGMA cache_size` reads back, in SQLite's own signed units.
    ///
    /// `None` until a caller sets one, when it is the pool's own size in
    /// kibibytes; afterwards it is the caller's number, so reading it always
    /// describes the cache the engine is actually keeping.
    pub(crate) cache_size: Option<i64>,
    /// Whether `LIKE` compares ASCII letters exactly.
    ///
    /// `PRAGMA case_sensitive_like`. Read by the binder's translation through
    /// `TreeCatalog::like_is_case_sensitive`, and the statement cache is
    /// emptied when it changes so a compiled `LIKE` is never run under the
    /// other setting.
    pub(crate) case_sensitive_like: bool,
    /// What `PRAGMA analysis_limit` was set to, in rows.
    ///
    /// Recorded and exceeded: `ANALYZE` walks the whole table, which is more
    /// than any cap asks for.
    pub(crate) analysis_limit: i64,
    /// What `PRAGMA writable_schema` was set to.
    ///
    /// Recorded and reported. There is nothing for it to unlock: the binder
    /// refuses a write to a reserved-prefix table whatever it says, and a
    /// module's shadow table is an ordinary table a write reaches without it.
    pub(crate) writable_schema: bool,
    /// Whether `SQLITE_DBCONFIG_DEFENSIVE` is in force.
    ///
    /// Off here and on in the shell, which is where SQLite draws the same line:
    /// the library defaults it off and its command-line tool turns it on. What
    /// it forbids is the two statements that can lose a database in one line -
    /// `PRAGMA journal_mode = OFF`, which stops protecting anything, and
    /// `PRAGMA writable_schema = ON`, which lets a caller write a schema row
    /// the engine will later try to parse.
    pub(crate) defensive: bool,
    /// The file system this database and everything beside it lives on.
    ///
    /// **One instance, held, rather than one made per call.** `OsVfs` is
    /// stateless, so the seven places that used to write `OsVfs::new()` were
    /// all the same file system and it did not matter which one they made. A
    /// `MemoryVfs` is not: each one is its own file system, so a journal that
    /// made its own would write pre-images into a directory the pool cannot
    /// see, and a reopen that made its own would find no file at all. Holding
    /// it is what lets `:memory:` be a database rather than a path the
    /// operating system refuses.
    vfs: std::sync::Arc<dyn inillucent_vfs::Vfs>,
    /// The imposter tables `.imposter` has made, and what each one reads.
    ///
    /// **Transient, and deliberately not in the catalog.** An imposter is a
    /// declaration over an index's own b-tree - `.imposter ix im` makes `im` a
    /// `WITHOUT ROWID` table whose columns are the index's entries - and it
    /// exists so a person can read an index directly when they are working out
    /// what is wrong with one. It is not a schema object: nothing writes it to
    /// the file, and it goes when the connection does, which is what SQLite's
    /// own `SQLITE_TESTCTRL_IMPOSTER` does with it.
    imposters: Vec<(TableInfo, SourceLayout, PagedTree)>,
    /// The authorizer every statement is bound under, when one is installed.
    ///
    /// `sqlite3_set_authorizer`'s subject: a callback the binder consults
    /// before it binds a read, a select or a function call, so an application
    /// embedding this engine can refuse a statement rather than run it. None
    /// means `AllowAll`, which is what a connection nobody has restricted has -
    /// and is the only case a compiled plan may be reused from the cache under,
    /// because re-running an authorizer is what makes its answer current.
    authorizer: Option<std::rc::Rc<dyn inillucent_sql::bind::Authorizer>>,
    /// Whether this connection refuses to write, set by `PRAGMA query_only`.
    ///
    /// Honoured rather than remembered: a caller sets it to make a mistake
    /// impossible, and one that recorded it and wrote anyway would be worse
    /// than an engine that refused the pragma outright.
    pub(crate) query_only: bool,
    /// Whether a trigger's own writes fire triggers, set by
    /// `PRAGMA recursive_triggers`.
    pub(crate) recursive_triggers: bool,
    /// The ceiling `PRAGMA max_page_count` set, in pages.
    pub(crate) max_page_count: i64,
    /// What `PRAGMA temp_store` reports.
    ///
    /// The *setting* rather than the state, which is what SQLite reports: this
    /// engine keeps temporary tables in memory whatever the number says, and
    /// the one value it cannot be - `FILE` - is refused rather than recorded.
    pub(crate) temp_store: i64,
    /// The collations an application registered, by upper-cased name.
    ///
    /// The comparator itself lives in `inillucent-value`'s custom table, which
    /// is process-wide because a `Collation` is a `Copy` handle carried through
    /// every key and every comparison. What is per-connection is the *name*:
    /// two connections may register different comparators under `MYCOLL`, and
    /// the binder resolves the name against this list before it falls back to
    /// the built-ins.
    collations: Vec<(String, Collation)>,
    /// The modules this connection knows, which is the built-in set.
    ///
    /// Held rather than looked up per statement because a module is registered
    /// once and asked many times, and because `CREATE VIRTUAL TABLE` has to find
    /// one by name before anything else can happen.
    registry: inillucent_ext::registry::Registry,
    /// The eponymous virtual tables the registry provides, derived once.
    ///
    /// **A catalog refresh happens after every DDL statement, and deriving
    /// these means connecting every eponymous module to read its declaration.**
    /// They are a function of the registry alone - not of the schema - so
    /// re-deriving them per refresh was work with no input that had changed,
    /// on the path the gate's `schema.index` measures. Rebuilt only when a
    /// module or a pragma is registered, which is at open and nowhere else.
    eponymous: Vec<inillucent_sql::catalog_view::TableInfo>,
    /// The virtual tables that have been connected, by folded name.
    virtual_tables: HashMap<Vec<u8>, vtab::Connected>,
    /// The indexes a module owns, by the root page of the table they index.
    ///
    /// **A vector index is a store plus a promise to keep it in step.** The
    /// store is an ordinary `inillucent_search` virtual table; the promise is
    /// this map and the code in `write` that reads it. It is rebuilt whenever
    /// the catalog changes, from the `source=` argument the engine itself wrote
    /// when the index was created - so an index survives a close without a
    /// second schema to keep in step with the first.
    vector_indexes: HashMap<u32, Vec<VectorIndex>>,
    /// Where the last `CREATE INDEX` spent its time, in nanoseconds.
    ///
    /// Scan, sort, uniqueness check, pack. On the harness's own type, in a
    /// test-only crate, and nothing in the engine consults it - the same shape
    /// as the write path's `execute_timed`, and for the same reason: `schema`
    /// is a gate this project has already been wrong about the cause of once.
    index_stages: std::cell::Cell<StageTimings>,
    /// How many times the catalog has changed.
    ///
    /// A plan compiled at one generation is not run at another: `execute_ddl`
    /// bumps this and empties the statement cache in the same breath, which is
    /// the TDD's "every plan cache is invalidated" made into two lines that
    /// cannot get out of step.
    catalog_generation: u64,

    /// The databases `ATTACH` has added beside the one this was opened on.
    ///
    /// **`main` is not one of these, deliberately.** The file a connection was
    /// opened on is not optional: it cannot be detached, it cannot be attached
    /// over, and it is the coordinator a cross-file commit is decided by. Every
    /// field above - `database`, `wal`, `entries`, `next_root` - is `main`'s,
    /// unchanged, which is what makes a connection that never attached anything
    /// the connection it was before `ATTACH` existed.
    ///
    /// Empty on almost every connection, and the read and write paths both
    /// check that before they look anything up.
    ///
    /// Numbered from **two**, because `temp` takes one. That is SQLite's own
    /// layout - `main`, `temp`, then the attachments in order - and taking it
    /// here is what lets `temp` be per connection without renumbering anything:
    /// schema one is *the running session's* temporary database, whichever that
    /// is, and every attachment keeps the number it was bound under.
    attached: Vec<Attached>,
    /// One temporary database per connection that has asked for one.
    ///
    /// **All at schema number one, told apart by whose they are.** A temporary
    /// object is one connection's own - `each_connection_has_its_own_temporary_database`
    /// grades exactly that against SQLite - so two connections' `temp.t` are two
    /// tables, and a statement reaches whichever belongs to the session running
    /// it.
    temps: Vec<Attached>,
    /// The session the statement now running belongs to.
    ///
    /// Set by every entry point from the connection that called it, so that
    /// `temp` resolves to that connection's temporary database and to no other.
    session: std::cell::Cell<u64>,
    /// The number the next connection takes.
    next_session: std::cell::Cell<u64>,
    /// The session `tables` and `catalog` were last derived for.
    ///
    /// **A connection's schema is its own.** `tables` holds the running
    /// session's temporary tables beside the shared ones, so when the session
    /// changes the derivation has to run again - once, on the change, rather
    /// than per statement. A connection that is the only one costs one
    /// comparison.
    tables_session: u64,
    /// Which schema each tree handle belongs to, for handles that are not
    /// `main`'s.
    ///
    /// Numbered as the binder numbers schemas: 0 is `main`, and *n* is
    /// `attached[n - 1]`. `main`'s handles are deliberately absent - a handle
    /// this map does not hold is `main`'s, which is what keeps a one-file
    /// connection's lookup a miss on an empty map rather than a hit on a full
    /// one.
    owner: HashMap<u32, usize>,
    /// The handle the next tree of an attached database is registered under.
    next_handle: u32,
    /// Which schema the DDL statement now running is about.
    ///
    /// **Statement-scoped, and set from the statement's own words.** Every DDL
    /// directive the binder produces carries a `database` - `CREATE TABLE
    /// aux.t` binds to one, `CREATE TEMP TABLE t` to another - and the
    /// primitives a schema change is built out of (`allocate_root`, `record`,
    /// `build_tree`, `seal`, `release_tree`) all have to write into that file
    /// rather than into `main`.
    ///
    /// It is a field rather than a parameter because those primitives are
    /// reached from forty call sites through a dozen intermediate functions,
    /// and a parameter threaded through all of them is forty chances to pass
    /// the wrong one. `execute_ddl` sets it from the directive and puts it back
    /// afterwards, so nothing outside one statement can observe it as anything
    /// but zero.
    ddl_schema: usize,
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
    /// Starts this connection's transaction counter above a number a log holds.
    ///
    /// **One counter, and now more than one log.** A file this connection
    /// attaches may hold higher transaction numbers than anything it has
    /// issued, and a number reused across the two would make a crashed run's
    /// records replay under a live transaction's commit - the resurrection
    /// `inillucent-wal` documents `Recovered::highest_txn` for.
    ///
    /// @param highest - the highest number the file's log carries
    fn raise_transactions_past(&self, highest: u64) {
        let next = highest.saturating_add(1);
        if self.next_txn.get() < next {
            self.next_txn.set(next);
        }
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
            return Some(self.path.clone());
        }
        self.schema_at(at).and_then(|held| held.path.clone())
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
    /// Returns which schema a tree handle belongs to; `MAIN` when it is
    /// `main`'s.
    ///
    /// **A handle this connection has not attached anything under is `main`'s.**
    /// The map holds only the handles of attached databases, so a connection
    /// that has attached nothing answers without hashing anything at all - which
    /// is what keeps this off the read path's bill.
    ///
    /// @param root - the handle
    fn schema_of(&self, root: u32) -> usize {
        if self.attached.is_empty() && self.temps.is_empty() {
            return MAIN;
        }
        self.owner.get(&root).copied().unwrap_or(MAIN)
    }

    /// Returns every schema number this connection holds, for the running
    /// session.
    ///
    /// `main` always; `temp` when this session has made one; then the
    /// attachments in the order they arrived. Another session's temporary
    /// database is not one of these, which is the whole of what makes it that
    /// session's own.
    fn schema_numbers(&self) -> Vec<usize> {
        let mut numbers = vec![MAIN];
        if self.schema_at(TEMP).is_some() {
            numbers.push(TEMP);
        }
        for nth in 0..self.attached.len() {
            numbers.push(FIRST_ATTACHED.saturating_add(nth));
        }
        numbers
    }

    /// Returns the session the statement now running belongs to.
    pub fn session(&self) -> u64 {
        self.session.get()
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
        self.session.set(session);
        // **The baseline is recorded here rather than when the number was
        // handed out (task-1962, A11).** `open_session` used to do both, and
        // `Database::session` therefore had to borrow the engine to open a
        // connection - which panicked when a callback asked for a second
        // connection while a statement was running. The number comes off a
        // counter the `Database` owns now, and the first statement that runs on
        // it records what `changed_ever` stood at. Nothing can have changed in
        // between: a connection that has run nothing has changed nothing.
        self.session_change_baseline
            .record_open_once(session, self.changed_ever.get());
        if self.tables_session == session {
            return;
        }
        self.tables_session = session;
        // A statement compiled for another connection may name that
        // connection's temporary trees, so it cannot be reused here either.
        let _ = self.rebuild_tables();
        self.refresh_catalog();
    }

    /// Returns a number for a connection that has just been opened.
    ///
    /// **Records `changed_ever`'s value at this instant as the new session's
    /// baseline**, which is what lets `total_changes()` answer only for rows
    /// this connection changed - see `session_change_baseline`. A caller that
    /// wants the *same* connection back across several calls uses
    /// `Connection::session` and `Database::connect_as` instead, which never
    /// reaches here and so never resets the baseline it already has.
    pub fn open_session(&self) -> u64 {
        let session = self.next_session.get();
        self.next_session.set(session.saturating_add(1));
        self.session_change_baseline
            .record_open(session, self.changed_ever.get());
        session
    }

    /// Makes the running session's temporary database, if it has not got one.
    ///
    /// **Lazily, because most connections never make a temporary object.** The
    /// file is a `MemoryVfs` of its own, so nothing about it reaches the
    /// directory the connection was opened in - which is what
    /// `a_temporary_table_is_not_in_the_file` is about - and it goes when the
    /// connection does.
    fn ensure_temp(&mut self) -> DbResult<()> {
        if self.schema_at(TEMP).is_some() {
            return Ok(());
        }
        let session = self.session.get();
        let vfs: std::sync::Arc<dyn inillucent_vfs::Vfs> =
            std::sync::Arc::new(inillucent_vfs::memory::MemoryVfs::new());
        let path = DbPath::from(format!("/temp/{session}.db").as_str());
        self.attach_file(vfs, path, None, b"temp".to_vec(), Some(session))
    }

    /// Returns the schema one number names, for the session now running.
    ///
    /// `None` for `main`, which is held as this type's own fields rather than as
    /// an element, and for a number nothing holds.
    ///
    /// @param at - the schema, as the binder numbers them
    fn schema_at(&self, at: usize) -> Option<&Attached> {
        match at {
            MAIN => None,
            TEMP => self
                .temps
                .iter()
                .find(|held| held.session == Some(self.session.get())),
            _ => self.attached.get(at.saturating_sub(FIRST_ATTACHED)),
        }
    }

    /// Returns the schema one number names, to write into.
    ///
    /// @param at - the schema, as the binder numbers them
    fn schema_at_mut(&mut self, at: usize) -> Option<&mut Attached> {
        let session = self.session.get();
        schema_of_index(&mut self.attached, &mut self.temps, session, at)
    }

    /// Returns one schema's file.
    ///
    /// Named `schema_file` rather than `file` because `ImportedDatabase::file`
    /// already answers a different question - the path this database is in.
    ///
    /// @param at - the schema, as the binder numbers them
    fn schema_file(&self, at: usize) -> Option<&Database> {
        if at == MAIN {
            return Some(&self.database);
        }
        self.schema_at(at).map(|held| &held.database)
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
            .schema_file(self.schema_of(root))
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
            return Some(std::rc::Rc::clone(&self.wal));
        }
        self.schema_at(at).map(|held| std::rc::Rc::clone(&held.wal))
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
            .unwrap_or_else(|| self.database.pool().uncommitted_handle())
    }

    /// Returns one schema's catalog rows.
    ///
    /// @param at - the schema, as the binder numbers them
    fn entries_of(&self, at: usize) -> &[Recorded] {
        if at == MAIN {
            return &self.entries;
        }
        match self.schema_at(at) {
            Some(held) => &held.entries,
            None => &[],
        }
    }

    /// Returns one schema's catalog rows, to add to.
    ///
    /// @param at - the schema, as the binder numbers them
    fn entries_of_mut(&mut self, at: usize) -> Option<&mut Vec<Recorded>> {
        if at == MAIN {
            return Some(&mut self.entries);
        }
        self.schema_at_mut(at).map(|held| &mut held.entries)
    }

    /// Returns the handle one schema's own `sqlite_schema` tree is read
    /// through.
    ///
    /// @param at - the schema, as the binder numbers them
    fn catalog_handle_of(&self, at: usize) -> u32 {
        if at == MAIN {
            return SCHEMA_VIEW_ROOT;
        }
        self.schema_at(at)
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
            let root = self.next_root;
            if root >= FIRST_ATTACHED_HANDLE {
                return Err(refusal(
                    "this database holds too many objects for one connection to name them all",
                ));
            }
            self.next_root = root.saturating_add(1);
            return Ok((root, root));
        }
        let handle = self.next_handle;
        if handle == u32::MAX {
            return Err(refusal(
                "this connection holds too many attached objects to name them all",
            ));
        }
        self.next_handle = handle.saturating_add(1);
        let held = self
            .schema_at_mut(at)
            .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
        let local = held.next_root;
        held.next_root = local.saturating_add(1);
        held.handles.insert(u64::from(local), handle);
        self.owner.insert(handle, at);
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
        self.schema_at(at)
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
        self.schema_at(at)
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
    fn pool_for(&self, root: u32) -> Option<&Pool> {
        Some(self.schema_file(self.schema_of(root))?.pool())
    }

    fn tree(&self, root: u32) -> Option<&PagedTree> {
        self.trees.get(&root)
    }

    fn layout(&self, root: u32) -> Option<&std::rc::Rc<SourceLayout>> {
        self.layouts.get(&root)
    }

    fn covering_candidates(&self, table_root: u32) -> Vec<u32> {
        self.covering.get(&table_root).cloned().unwrap_or_default()
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

    fn like_is_case_sensitive(&self) -> bool {
        self.case_sensitive_like
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

/// Returns the value of one `name=value` module argument, when it is there.
///
/// **Only the arguments the engine wrote itself.** Deriving a module's columns
/// from its `CREATE` text would be a second implementation of its argument
/// grammar; reading back a marker this engine put there is not, and there is
/// nowhere else durable to keep the link between an index and the table it
/// indexes.
///
/// @param arguments - the module's arguments, as written
/// @param name - the argument to find
fn argument_of(arguments: &[Vec<u8>], name: &[u8]) -> Option<Vec<u8>> {
    for argument in arguments {
        let text = String::from_utf8_lossy(argument);
        let Some((key, value)) = text.split_once('=') else {
            continue;
        };
        if key.trim().as_bytes().eq_ignore_ascii_case(name) {
            let value = value.trim();
            if value.is_empty() {
                return None;
            }
            return Some(value.as_bytes().to_vec());
        }
    }
    None
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
    /// Returns where the last `CREATE INDEX` spent its time.
    ///
    /// Milliseconds per stage, rendered for a report.
    pub fn build_stages(&self) -> String {
        let timings = self.index_stages.get();
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

    /// Returns the last `CREATE INDEX`'s stages in nanoseconds.
    ///
    /// **The raw numbers, so a harness can take a median rather than report one
    /// round.** `build_stages` renders whatever the *last* round happened to
    /// cost, and a single round of a 35 ms statement moves by several
    /// milliseconds - enough that reading the stages off one round and the
    /// total off thirty says the two do not add up when they do.
    ///
    pub fn build_stage_nanos(&self) -> StageTimings {
        self.index_stages.get()
    }

    /// Returns the log, so a caller can read its counters.
    pub fn wal(&self) -> &Wal {
        &self.wal
    }

    /// Returns how many bytes of a script the first statement uses.
    ///
    /// The parser's own count, including the terminating semicolon and the
    /// trivia after it, so a caller stepping a script lands on the next
    /// statement rather than on the space before it.
    ///
    /// @param sql - the script, positioned at the statement to measure
    pub fn statement_length(&self, sql: &str) -> DbResult<usize> {
        let parsed = parse_next_statement(sql.as_bytes(), 0, &self.limits).map_err(refused)?;
        Ok(parsed.consumed)
    }

    /// Returns the schema's generation, which changes when the schema does.
    pub fn schema_generation(&self) -> u64 {
        self.catalog_generation
    }

    /// Rereads the schema from the file, discarding compiled statements.
    ///
    /// The catalog is a snapshot and a plan is compiled against one, so a
    /// reload is a new generation and an empty statement cache - not an edit of
    /// the snapshot the held plans are still reading.
    pub fn reload_catalog(&mut self) -> DbResult<()> {
        self.reload_entries()?;
        Ok(())
    }

    /// Returns what the write path has done to every tree, added up.
    ///
    /// The counters, not the clock. For a write the counters are the story: a
    /// page compacted is a whole page image in the log, and a tree that
    /// compacts once per statement is doing work no timing will explain on its
    /// own.
    pub fn write_stats(&self) -> inillucent_tree::write::WriteStats {
        let mut total = inillucent_tree::write::WriteStats::default();
        for tree in self.trees.values() {
            let held = tree.write_stats();
            total.inserted = total.inserted.saturating_add(held.inserted);
            total.deleted = total.deleted.saturating_add(held.deleted);
            total.updated_in_place = total.updated_in_place.saturating_add(held.updated_in_place);
            total.compactions = total.compactions.saturating_add(held.compactions);
            total.splits = total.splits.saturating_add(held.splits);
            total.merges = total.merges.saturating_add(held.merges);
        }
        total
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
        self.registry.register_module(module);
        // The eponymous list is cached because building it connects every
        // module; a new module invalidates it, and nothing else does.
        self.eponymous.clear();
        self.refresh_catalog();
        Ok(())
    }

    /// Returns the catalog rows of `main`, for the rebuild to replay.
    ///
    /// @returns one entry per object, in catalog order
    pub(crate) fn main_entries(&self) -> Vec<inillucent_catalog::paged::SchemaEntry> {
        self.entries.iter().map(|held| held.entry.clone()).collect()
    }

    /// Returns what `PRAGMA user_version` would answer.
    pub(crate) fn user_version(&self) -> i32 {
        self.database.user_version()
    }

    /// Returns what `PRAGMA application_id` would answer.
    pub(crate) fn application_id(&self) -> i32 {
        self.database.application_id()
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
            std::sync::Arc::clone(&self.vfs),
            self,
            destination,
            self.page_size,
            self.frames,
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
        let free = self.database.free_pages();
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

    /// Sets what a commit waits for.
    ///
    /// @param policy - the `synchronous` setting
    pub fn set_synchronous(&self, policy: Synchronous) {
        self.wal.set_synchronous(policy);
    }

    /// Closes the file and opens it again, from the catalog alone.
    ///
    /// **The test that makes the persisted statistics load-bearing.** Every tree
    /// handle is rebuilt from the catalog row's leftmost leaf, leaf count and
    /// row count rather than from anything this process remembers, so a file
    /// whose statistics were wrong answers differently after a reopen - which is
    /// the failure the numbers exist to prevent, made visible.
    ///
    /// It is on the harness rather than in the engine because the engine's own
    /// open path is Phase 5's consumer story. What this proves is that the
    /// *format* carries what an open needs, which is the part Phase 4 owes.
    pub fn reopen(&mut self) -> DbResult<()> {
        self.checkpoint()?;
        let path = self.path.clone();
        let frames = self.frames;
        let vfs = std::sync::Arc::clone(&self.vfs);
        let db_path = DbPath::new(path.to_string_lossy().as_ref());
        // **The old handle lets the file go before the new one asks for it.**
        // Since the engine takes real file locks, a handle that is still holding
        // one is a writer as far as the open path is concerned, and the open
        // would wait out its whole busy budget and then report the file busy -
        // against a lock this same call is about to drop. The checkpoint above
        // has already made the file current, so there is nothing left for the
        // lock to protect.
        self.database.end_access()?;
        // The old handle's file is closed before the new one opens it, because
        // two `Database`s over one path is two page caches over one file.
        let database = {
            let replacement = Database::open(vfs.as_ref(), &db_path, frames.max(64))?;
            std::mem::replace(&mut self.database, replacement)
        };
        drop(database);

        let stored = read_catalog(
            self.database.pool(),
            &attach_catalog(self.database.pool(), self.database.catalog_root())?,
        )?;
        let mut trees = HashMap::new();
        let mut entries: Vec<Recorded> = Vec::new();
        for (position, entry) in stored.into_iter().enumerate() {
            let rowid = position.saturating_add(1) as i64;
            // The identifier this tree is registered under, carried across so the
            // plans and layouts this handle already holds keep pointing at the
            // same trees.
            //
            // **This comment used to say the identifier was "this process's own
            // bookkeeping and is not in the file", and that is no longer true.**
            // It was never quite true: every logical row record
            // in the log carries it, so a reader that numbered trees differently
            // would send recovery's records to the wrong tree. It is in the
            // catalog now, `open` reads it from there, and the lookup below
            // agrees with what `open` would derive rather than merely with what
            // this handle happens to remember.
            let root = self
                .entries
                .iter()
                .find(|held| held.entry.kind == entry.kind && held.entry.name == entry.name)
                .map(|held| held.root)
                .unwrap_or(0);
            if entry.root.is_none() || root == 0 {
                entries.push(Recorded { rowid, root, entry });
                continue;
            }
            let Some(columns) = self.trees.get(&root).map(|tree| tree.columns().to_vec()) else {
                entries.push(Recorded { rowid, root, entry });
                continue;
            };
            let key_columns = self
                .trees
                .get(&root)
                .map(PagedTree::key_columns)
                .unwrap_or(1);
            let tree = PagedTree::attach(
                self.database.pool(),
                u64::from(root),
                entry.root,
                columns,
                key_columns,
                entry.stats.leaf_count,
                entry.stats.row_count,
            )?;
            trees.insert(root, tree);
            entries.push(Recorded { rowid, root, entry });
        }
        // The catalog itself, which the meta page points at rather than a row.
        let catalog_tree = attach_catalog(self.database.pool(), self.database.catalog_root())?;
        trees.insert(SCHEMA_VIEW_ROOT, catalog_tree);
        self.trees = trees;
        self.entries = entries;
        self.wal = std::rc::Rc::new(Wal::open(
            std::sync::Arc::clone(&self.vfs),
            &db_path,
            self.database.uuid(),
            FIRST_LSN,
            1,
            WalOptions::default(),
        )?);
        self.database
            .pool()
            .set_durable_lsn(self.wal.write_ahead_point());
        let_the_pool_ask_the_log(self.database.pool(), &self.wal);
        self.rebuild_tables()?;
        self.refresh_catalog();
        Ok(())
    }

    /// Binds one statement against the imported schema.
    ///
    /// @param sql - the statement text
    pub fn bind(&self, sql: &str) -> DbResult<BoundStatement> {
        let parsed = self.parse_once(sql)?;
        let bound = self.bind_parsed(sql, &parsed);
        self.recycle(parsed);
        bound
    }

    /// Binds a statement somebody has already parsed.
    ///
    /// **So that a compile parses once.** `compile` has to look at the parse to
    /// decide whether the statement is an `EXPLAIN` - the binder's job is the
    /// statement being explained, not the explaining - and it then called
    /// `bind`, which parsed the same text a second time. On `SELECT 1` that was
    /// 270 ns of a 1,145 ns compile spent producing an arena that was thrown
    /// away, and `prepare.trivial` pays a compile every iteration.
    ///
    /// @param sql - the statement text, for diagnostics and spans
    /// @param parsed - the parse to bind
    fn bind_parsed(
        &self,
        sql: &str,
        parsed: &inillucent_sql::parser::ParsedStatement,
    ) -> DbResult<BoundStatement> {
        let fallback = AllowAll;
        let authorizer: &dyn inillucent_sql::bind::Authorizer = match &self.authorizer {
            Some(held) => held.as_ref(),
            None => &fallback,
        };
        let externals = self.external_functions();
        let mut binder = Binder::new(&self.catalog, &parsed.ast, authorizer)
            .with_source(sql.as_bytes())
            .with_functions(&externals)
            .with_collations(&self.collations)
            .with_limits(&self.limits)
            .with_foreign_keys(self.foreign_keys, self.defer_foreign_keys);
        binder.bind_statement(&parsed.statement).map_err(refused)
    }

    /// Parses, plans and runs one statement of any kind.
    ///
    /// A `SELECT` answers with rows; an `INSERT`, `UPDATE` or `DELETE` answers
    /// with a count and whatever `RETURNING` asked for. One entry point rather
    /// than two, because a corpus record does not say which it is and a harness
    /// that had to guess would be guessing from the SQL text.
    ///
    /// @param sql - the statement text
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute_any(&mut self, sql: &str, params: &Params) -> DbResult<Outcome> {
        let cached = self.compiled(sql)?;
        self.execute_compiled(&cached, params)
    }

    /// Compiles one statement and hands back the handle, without running it.
    ///
    /// **So that a caller can take the compile out of a timed region**, which is
    /// where SQLite's already is: `sqlite_bench.c` calls `sqlite3_prepare_v2`
    /// before it reads the clock and then resets and re-binds inside the loop.
    /// A harness that looked its statement up per iteration would be timing a
    /// hash of the SQL text that the other arm does not pay.
    ///
    /// @param sql - the statement text
    pub fn prepare_statement(&self, sql: &str) -> DbResult<Statement> {
        Ok(Statement {
            cached: std::cell::RefCell::new(self.compiled(sql)?),
            sql: sql.to_string(),
            generation: std::cell::Cell::new(self.schema_generation()),
        })
    }

    /// Runs a statement [`ImportedDatabase::prepare_statement`] compiled.
    ///
    /// **The plan is compiled again when the schema has moved under it
    /// (task-1932).** A plan is built against a snapshot of the catalog, and a
    /// statement held across a `CREATE TABLE`, a `DROP`, an `ALTER` or a
    /// `REINDEX` is holding one that describes trees that are not there any
    /// more. `Connection::step` has checked this since it existed; this
    /// entry point, which the gates and the profiles run their statements
    /// through, did not - so the two halves of the same public API disagreed
    /// about whether an already-prepared statement follows a schema change.
    /// SQLite's own `sqlite3_step` reprepares, and so does this.
    ///
    /// @param statement - the handle
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute_statement(
        &mut self,
        statement: &Statement,
        params: &Params,
    ) -> DbResult<Outcome> {
        let held = self.current_plan(statement)?;
        self.execute_compiled(&held, params)
    }

    /// Returns a statement's plan, compiling it again if the schema has moved.
    ///
    /// @param statement - the handle
    fn current_plan(&self, statement: &Statement) -> DbResult<std::rc::Rc<Cached>> {
        let generation = self.schema_generation();
        if statement.generation.get() != generation {
            let fresh = self.compiled(&statement.sql)?;
            *statement.cached.borrow_mut() = fresh;
            statement.generation.set(generation);
        }
        Ok(std::rc::Rc::clone(&statement.cached.borrow()))
    }

    /// Runs one statement and reports where its time went.
    ///
    /// **Two numbers, because there are two halves and they are fixed in
    /// different places.** `find` is the query that decides which rows change -
    /// an ordinary planned query, whose cost is the operator chain and the
    /// descent. `apply` is everything after: compiling the assignments, reading
    /// the rows, maintaining the indexes and writing the tree.
    ///
    /// This exists because the write gate misses and a guess about which half is
    /// expensive is a guess this project has been wrong about before. It is on
    /// the harness's own type, in a test-only crate, and nothing in the engine
    /// consults it.
    ///
    /// @param statement - a handle from `prepare_statement`
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute_timed(
        &mut self,
        statement: &Statement,
        params: &Params,
    ) -> DbResult<(u128, u128)> {
        let cached = self.current_plan(statement)?;
        let found = std::time::Instant::now();
        let rows = match &*cached {
            Cached::Nothing
            | Cached::Ddl(_)
            | Cached::QueryPlan(_)
            | Cached::Program(_)
            | Cached::VirtualUpdate(..)
            | Cached::VirtualDelete(..)
            | Cached::VirtualInsert(_)
            | Cached::Select(..)
            | Cached::Insert(_, None, _) => Vec::new(),
            // This harness measures a fresh build on purpose - see the doc
            // comment - so it keeps calling `run_any_prepared` directly
            // rather than `query`'s slot, exactly as it did before Stage 3.
            Cached::Insert(_, Some(query), _) => {
                physical::run_any_prepared(&query.plan, self, &query.prepared, params)?.0
            }
            Cached::Update(_, query, _, _) | Cached::Delete(_, query) => {
                if let Some(key) = physical::rowid_seek_key(&query.plan, params)? {
                    vec![vec![key]]
                } else {
                    physical::run_any_prepared(&query.plan, self, &query.prepared, params)?.0
                }
            }
        };
        let find = found.elapsed().as_nanos();
        let applied = std::time::Instant::now();
        match &*cached {
            Cached::Nothing => {}
            Cached::Ddl(sql) => {
                self.execute_ddl(sql)?;
            }
            // Rendered when it was compiled, so there is nothing to apply and
            // nothing to time. It is here to be exhaustive rather than to be
            // measured: a plan description is not a workload.
            Cached::QueryPlan(_) | Cached::Program(_) => {}
            // A module's own write, which this harness does not time: what it
            // costs is the module's business and not the engine's.
            Cached::VirtualDelete(..) | Cached::VirtualUpdate(..) => {}
            Cached::VirtualInsert(statement) => {
                self.insert_into_module(statement, params)?;
            }
            Cached::Select(plan, prepared, _) => {
                physical::run_any_prepared(plan, self, prepared, params)?;
            }
            Cached::Insert(statement, ..) => {
                self.write(params, Vec::new(), |target, params| {
                    dml::insert(statement, target, params, &rows)
                })?;
            }
            Cached::Update(statement, _, _, setup) => {
                self.write(params, Vec::new(), |target, params| {
                    dml::update_cached(statement, target, params, &rows, setup)
                })?;
            }
            Cached::Delete(statement, ..) => {
                self.write(params, Vec::new(), |target, params| {
                    dml::delete(statement, target, params, &rows)
                })?;
            }
        }
        Ok((find, applied.elapsed().as_nanos()))
    }

    /// One foreign key's violation query, with what it is about.
    ///
    /// The child and parent names and the key's own id are carried alongside
    /// the SQL because `PRAGMA foreign_key_check` reports all three and the
    /// query itself only produces a rowid.
    fn violation_queries(&self, only: Option<&str>) -> DbResult<Vec<ViolationQuery>> {
        let mut queries = Vec::new();
        for child in &self.tables {
            if child.kind != inillucent_sql::catalog_view::TableKind::Table
                || child.folded.starts_with(b"sqlite_")
            {
                continue;
            }
            if only.is_some_and(|name| child.folded != name.as_bytes()) {
                continue;
            }
            for key in &child.foreign_keys {
                let Some(parent) = self
                    .tables
                    .iter()
                    .find(|candidate| candidate.folded == key.parent_folded)
                else {
                    continue;
                };
                let Some(sql) =
                    inillucent_sql::foreign_key::violation_query(child, parent, key, b"main")
                else {
                    continue;
                };
                queries.push(ViolationQuery {
                    sql,
                    child: child.name.clone(),
                    parent: parent.name.clone(),
                    key: u16::try_from(key.id).unwrap_or_default(),
                });
            }
        }
        Ok(queries)
    }

    /// Runs one query the engine wrote for itself, and returns its rows.
    ///
    /// **The engine asking itself a question.** A foreign-key check *is* a
    /// query, and running it through the ordinary compile-and-execute path is
    /// what makes it use the ordinary indexes - and what stops there being a
    /// second, hand-written scan that has to be kept in step with the first.
    ///
    /// @param sql - the statement the engine generated
    pub(crate) fn query_internally(&mut self, sql: &str) -> DbResult<Vec<Vec<OwnedDatum>>> {
        Ok(self.execute_any(sql, &Params::default())?.rows)
    }

    /// Applies the actions of every key that can lead back to its own table.
    ///
    /// **A cyclic action cannot be inlined**, because the body would have to
    /// appear once per level the data happens to be deep and that is not known
    /// when the statement is compiled. The binder therefore stops a cascade at
    /// the level it can see - the rows that pointed directly at the row that
    /// went - and this takes what that leaves: every row whose key now has no
    /// parent, repeated until nothing changes.
    ///
    /// It terminates because every pass either changes a row or stops, and a
    /// pass only ever removes a row or clears a key.
    ///
    /// It runs after the statement rather than inside it, and only on a schema
    /// that has such a key, so a schema without one pays a flag test.
    pub(crate) fn settle_foreign_keys(&mut self) -> DbResult<()> {
        if !self.foreign_keys || !self.has_cyclic_foreign_keys() {
            return Ok(());
        }
        let mut statements = Vec::new();
        for child in &self.tables {
            if child.kind != inillucent_sql::catalog_view::TableKind::Table {
                continue;
            }
            for key in &child.foreign_keys {
                if !key.cyclic {
                    continue;
                }
                let Some(parent) = self
                    .tables
                    .iter()
                    .find(|candidate| candidate.folded == key.parent_folded)
                else {
                    continue;
                };
                if let Some(sql) =
                    inillucent_sql::foreign_key::sweep_statement(child, parent, key, b"main")
                {
                    statements.push(sql);
                }
            }
        }
        if statements.is_empty() {
            return Ok(());
        }
        for _ in 0..MAX_SWEEP_PASSES {
            // The running total is what says whether a pass did anything: it
            // moves as each statement finishes, so comparing it across a pass
            // asks exactly "did any of these change a row" without the sweep
            // having to count them itself.
            let before = self.changed_ever.get();
            for sql in &statements {
                self.execute_any(sql, &Params::default())?;
            }
            if self.changed_ever.get() == before {
                return Ok(());
            }
        }
        Err(refusal(
            "a foreign key's action did not settle; the schema may have a cycle that cannot resolve",
        ))
    }

    /// Parses one statement into the connection's own arena, and puts it back.
    ///
    /// **One arena, borrowed for the length of a compile.** The arena is taken
    /// out of the cell, filled, and the *previous* one is returned to the cell
    /// once the caller has finished with the parse - which is what
    /// [`ImportedDatabase::recycle`] is for. A caller that forgets to recycle
    /// loses the capacity and nothing else: the next parse makes a fresh arena.
    ///
    /// @param sql - the statement text
    fn parse_once(&self, sql: &str) -> DbResult<inillucent_sql::parser::ParsedStatement> {
        let arena = self.scratch_ast.borrow_mut().take().unwrap_or_default();
        inillucent_sql::parser::parse_next_statement_into(sql.as_bytes(), 0, &self.limits, arena)
            .map_err(refused)
    }

    /// Returns the named parameters one statement declares, with their indexes.
    ///
    /// **A question about the text, answered by parsing it.** The compiled form
    /// does not carry the names - a plan is cached by its SQL and the values
    /// arrive later - and the caller that needs them is a *shell*, binding what
    /// a person typed into `.parameter set`. Parsing again costs one parse of
    /// one statement and keeps the name table out of every cached plan.
    ///
    /// @param sql - the statement text
    pub fn parameter_names(&self, sql: &str) -> DbResult<Vec<(Vec<u8>, u32)>> {
        let parsed = self.parse_once(sql)?;
        let names = parsed.parameters.names.clone();
        self.recycle(parsed);
        Ok(names)
    }

    /// Returns the highest parameter number a statement uses.
    ///
    /// What `sqlite3_bind_parameter_count` answers, and what a bind has to be
    /// checked against: an index above it is `SQLITE_RANGE` rather than a slot
    /// nobody will ever read.
    ///
    /// It is a parse rather than a lookup because the compiled plan does not
    /// carry the number - `Cached` has thirteen variants and none of them has a
    /// place to put it. The parse reuses the recycled arena, which is most of
    /// what a parse costs, and it happens once per `prepare` rather than once
    /// per execution: a statement prepared once and stepped a million times
    /// pays it once.
    ///
    /// @param sql - the statement text
    pub fn parameter_count(&self, sql: &str) -> DbResult<u32> {
        let parsed = self.parse_once(sql)?;
        let count = parsed.parameters.count;
        self.recycle(parsed);
        Ok(count)
    }

    /// Puts a finished parse's arena back for the next statement to fill.
    ///
    /// @param parsed - the parse nothing holds a reference into any more
    fn recycle(&self, parsed: inillucent_sql::parser::ParsedStatement) {
        *self.scratch_ast.borrow_mut() = Some(parsed.ast);
    }

    /// Returns whether a compiled statement changes the database.
    ///
    /// A read takes a shared lock and a write an exclusive one, so the answer
    /// decides which. A directive is counted as a write: `CREATE TABLE` and
    /// `PRAGMA user_version = 1` both change the file, and the ones that do not
    /// pay a lock they did not need rather than skip one they did.
    fn writes_of(cached: &Cached) -> bool {
        !matches!(
            cached,
            Cached::Select(..) | Cached::QueryPlan(_) | Cached::Program(_) | Cached::Nothing
        )
    }

    /// Returns whether the file lock is kept between transactions.
    pub(crate) fn locking_exclusive(&self) -> bool {
        self.locking_exclusive
    }

    /// Chooses whether the file lock is kept between transactions.
    ///
    /// Dropping to `normal` releases the lock immediately, which is the moment
    /// a second process may open the file; raising to `exclusive` takes it at
    /// the next statement rather than here, because taking it now would make a
    /// pragma block on a lock the caller has not asked to wait for.
    ///
    /// @param exclusive - whether to keep the lock
    pub(crate) fn set_locking_exclusive(&mut self, exclusive: bool) -> DbResult<()> {
        self.locking_exclusive = exclusive;
        if !exclusive && self.batch.get().is_none() {
            self.database.end_access()?;
        }
        Ok(())
    }

    /// Takes the lock a statement needs and reloads if the file has moved.
    ///
    /// **Called before every statement**, so a connection that has been idle
    /// while another process wrote sees the new database rather than its own
    /// cache of the old one. Under `exclusive` the lock is already held and
    /// this is a comparison of two integers.
    ///
    /// @param writing - whether the statement changes the database
    pub(crate) fn enter(&mut self, writing: bool) -> DbResult<()> {
        self.running = self.running.saturating_add(1);
        // **A transaction holds its lock from the first write to the commit.**
        // Once inside one, the retry loop's release would open a window another
        // process could write through - see `Database::begin_write_within`.
        let inside = self.batch.get().is_some() || self.running > 1;
        let reloaded = if writing {
            self.database.begin_write_within(!inside)?
        } else {
            self.database.begin_read()?
        };
        // **The pages are not the whole cache.** `begin_read` throws away the
        // pool when another process has committed; the *schema* this connection
        // read at open is just as stale, and a connection that kept it would
        // write its own catalog tree over the one the other process just built -
        // which is a lost table rather than a stale read. `reload_catalog` is
        // the same reread `ATTACH` does.
        if reloaded && !inside {
            self.reload_catalog()?;
            // **And the modules hear that somebody else committed
            // (task-1932, M2).** A module's own state is derived from its
            // shadow tables, which are ordinary trees another connection can
            // have written; this is the one moment the engine knows that
            // happened.
            self.committed_elsewhere_modules();
        }
        Ok(())
    }

    /// Releases the lock when nothing holds the connection to the file.
    ///
    /// A transaction is open, so nothing is released: a `BEGIN` that let the
    /// file go between its statements would be a transaction another process
    /// could write through the middle of.
    pub(crate) fn leave(&mut self) -> DbResult<()> {
        self.running = self.running.saturating_sub(1);
        if self.running > 0 || self.locking_exclusive || self.batch.get().is_some() {
            return Ok(());
        }
        // **Durable before the file is let go, and this is the whole cost of
        // `locking_mode = normal`.** A connection that released the lock with
        // dirty pages would leave the file describing a database without the
        // statement that just succeeded - and the next process to write would
        // build on that file and overwrite the statement for good. It is not a
        // stale read; it is a lost write, and it is what the first version of
        // this did eight times in ten under two concurrent writers.
        //
        // Under `exclusive`, which is the default, the lock is never let go and
        // none of this runs: the checkpoint happens when the connection closes,
        // as it always did.
        if self.database.pool().lock_level() != inillucent_vfs::FileLock::None {
            self.checkpoint()?;
        }
        self.database.end_access()
    }

    /// Returns how the pre-commit state is protected.
    pub(crate) fn journal_mode(&self) -> inillucent_pool::journal::JournalMode {
        self.journal_mode
    }

    /// Changes how the pre-commit state is protected.
    ///
    /// **The database is checkpointed on the way through**, which is not a
    /// tidy-up: the two schemes protect different things, and a switch made
    /// with uncommitted state in either of them would leave a file neither of
    /// them could recover. SQLite refuses the switch inside a transaction for
    /// the same reason.
    ///
    /// @param mode - the mode to switch to
    pub(crate) fn set_journal_mode(
        &mut self,
        mode: inillucent_pool::journal::JournalMode,
    ) -> DbResult<()> {
        if mode == self.journal_mode {
            return Ok(());
        }
        if self.batch.get().is_some() {
            return Err(refusal(
                "cannot change PRAGMA journal_mode from within a transaction",
            ));
        }
        self.checkpoint()?;
        self.journal_mode = mode;
        // **WAL is the one mode the file remembers.** SQLite writes a
        // read/write version of 2 into its header for a WAL database and 1 for
        // everything else, so a reopen comes back in WAL and comes back at the
        // connection's default for any of the rollback modes. Recording it here
        // is what makes `PRAGMA journal_mode = wal` outlive the connection that
        // asked - without it, a reopen of a WAL database answered `delete` and
        // would have started writing pre-images beside a log.
        self.database
            .set_wal_mode(mode == inillucent_pool::journal::JournalMode::Wal);
        // And checkpointed again, because the meta record reaches the file at a
        // checkpoint and the one above ran before the flag was set. Without
        // this second one the flag is written only if something else forces a
        // checkpoint later, so `PRAGMA journal_mode = wal` followed by a clean
        // close reopened as `delete`.
        self.checkpoint()?;
        // A VFS of its own rather than the schema's, because the journal opens
        // one file by name and `OsVfs` is stateless - the same reasoning that
        // lets `create` and `open` each make their own.
        let held: std::sync::Arc<dyn inillucent_vfs::Vfs> = std::sync::Arc::clone(&self.vfs);
        let journal = journal_for(mode).map(|protection| {
            inillucent_pool::journal::Journal::new(
                held,
                &DbPath::new(self.path.to_string_lossy().as_ref()),
                protection,
                self.page_size,
            )
        });
        self.database.pool().set_journal(journal);
        // **`PRAGMA journal_mode` names the connection, not one file of it.**
        // `checkpoint_attached` writes an attached file's pages in place
        // exactly as `main`'s checkpoint does, so an attachment left on its
        // old journal here would keep the interrupted-checkpoint defect open
        // for every database this connection holds but the one the pragma
        // named. Each attachment gets its own `Journal`, over its own path and
        // its own file's page size, because an attached file can have been
        // created at a page size that differs from this connection's.
        for held in self.attached.iter_mut() {
            let Some(path) = held.path.as_ref() else {
                // `:memory:` has no file to checkpoint into, so nothing here
                // needs protecting.
                continue;
            };
            let attached_path = DbPath::new(path.to_string_lossy().as_ref());
            let attached_vfs: std::sync::Arc<dyn inillucent_vfs::Vfs> =
                std::sync::Arc::clone(&held.vfs);
            let page_size = held.database.page_size();
            let attached_journal = journal_for(mode).map(|protection| {
                inillucent_pool::journal::Journal::new(
                    attached_vfs,
                    &attached_path,
                    protection,
                    page_size,
                )
            });
            held.database.pool().set_journal(attached_journal);
        }
        Ok(())
    }

    /// Turns the automatic index on or off, which `PRAGMA automatic_index` does.
    ///
    /// It is its own method rather than a call to `disable_levers` because that
    /// one only ever turns levers *off* - it is the measurement harness's entry
    /// point, and an A/B arm never turns one back on. A pragma has to do both.
    ///
    /// @param on - whether the planner may build one
    pub(crate) fn set_automatic_index(&mut self, on: bool) {
        let mask = self.levers.disabled();
        self.levers = Levers::without(if on {
            mask & !Levers::AUTOMATIC_INDEX
        } else {
            mask | Levers::AUTOMATIC_INDEX
        });
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
        self.registry.function(name, argc)
    }

    /// Checks every deferred foreign key, and reports the first violation.
    ///
    /// **A full check rather than a running count.** SQLite keeps a counter of
    /// outstanding violations and moves it as rows appear and disappear; a
    /// counter that drifts by one reports a violation that is not there, or
    /// misses one that is, and neither is visible until a commit fails for a
    /// reason nobody can reproduce. Asking the question directly costs a query
    /// per deferred key per commit and cannot drift.
    pub(crate) fn check_deferred_foreign_keys(&mut self) -> DbResult<()> {
        if !self.foreign_keys || !self.has_deferred_foreign_keys() {
            return Ok(());
        }
        for query in self.violation_queries(None)? {
            if self.query_internally(&query.sql)?.is_empty() {
                continue;
            }
            return Err(DbError::new(inillucent_base::ExtendedCode(
                inillucent_sql::dml::codes::FOREIGN_KEY,
            ))
            .with_message("FOREIGN KEY constraint failed")
            .with_detail(format!(
                "deferred key {} of {}",
                query.key,
                String::from_utf8_lossy(&query.child)
            )));
        }
        Ok(())
    }

    /// Reports whether any key's checks are waiting for the commit.
    fn has_deferred_foreign_keys(&self) -> bool {
        self.defer_foreign_keys
            || self
                .tables
                .iter()
                .any(|table| table.foreign_keys.iter().any(|key| key.is_deferred()))
    }

    /// Reports whether any key can lead back to the table that declares it.
    fn has_cyclic_foreign_keys(&self) -> bool {
        self.tables
            .iter()
            .any(|table| table.foreign_keys.iter().any(|key| key.cyclic))
    }

    /// Returns the keys a write will change.
    ///
    /// A rowid-equality `WHERE` is answered from the plan itself - see
    /// `physical::rowid_seek_key` - without asking `query`'s slot at all; that
    /// shape is already a single lookup, so Stage 3's saving is on the range
    /// and index shapes instead. Everything else runs through
    /// [`ImportedDatabase::run_cached_query`], the slot-try/build-once/reuse
    /// mechanism `execute_select_cached` gives a `SELECT`.
    ///
    /// **Runs under `&self` and returns before `self.write` takes `&mut
    /// self`.** `query.slot.try_borrow_mut()` is taken and dropped inside
    /// `run_cached_query`, entirely before this returns, so nothing about it
    /// is still borrowed when the write that follows needs `&mut self`.
    ///
    /// @param query - the keys query and its compiled-chain cache
    /// @param params - the bound parameters
    fn keys_of(&self, query: &CachedQuery, params: &Params) -> DbResult<Vec<Vec<OwnedDatum>>> {
        if let Some(key) = physical::rowid_seek_key(&query.plan, params)? {
            return Ok(vec![vec![key]]);
        }
        self.run_cached_query(&query.plan, &query.prepared, &query.slot, params)
    }
}

/// How many times the cyclic sweep repeats before it gives up.
///
/// One pass per level of the deepest chain in the data. A tree deeper than this
/// is a tree with a million levels, which is a different problem.
const MAX_SWEEP_PASSES: usize = 1_000_000;

/// One foreign key's violation query, and what it is about.
struct ViolationQuery {
    /// The `SELECT` that finds the rows with no parent.
    sql: String,
    /// The child table's name, which the pragma reports.
    child: Vec<u8>,
    /// The parent table's name, which the pragma reports.
    parent: Vec<u8>,
    /// The key's position in its table, which the pragma reports as `fkid`.
    key: u16,
}

/// A statement compiled once and run many times.
///
/// Opaque on purpose: what is inside is the engine's business, and a caller that
/// could see it would be a caller that could be broken by a plan shape changing.
pub struct Statement {
    /// The compiled plan, replaced when the schema moves under it.
    ///
    /// A `RefCell` because [`ImportedDatabase::execute_statement`] takes the
    /// statement by shared reference and every caller holds it across many
    /// executions; the reprepare has to happen in place or the signature would
    /// have to change under all of them.
    cached: std::cell::RefCell<std::rc::Rc<Cached>>,
    /// The statement's text, so it can be compiled again.
    sql: String,
    /// The schema generation `cached` was compiled against.
    generation: std::cell::Cell<u64>,
}

/// What running one statement produced.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// The rows a `SELECT` answered, or the rows `RETURNING` named.
    pub rows: Vec<Vec<OwnedDatum>>,
    /// The result column names, for a `SELECT`.
    pub names: Vec<String>,
    /// What a write changed.
    pub changes: Changes,
}

/// Turns a parse or bind failure into a database error, keeping its kind.
///
/// **The message is exactly what it was**; what this adds is that a refusal the
/// binder marked `Unsupported` - "a construct the grammar has but this phase
/// does not implement" - arrives carrying that fact, where every conversion
/// site used to flatten it into an ordinary `SQLITE_MISUSE`.
///
/// It matters because this engine is deliberately incomplete, and a caller in
/// front of it has to tell "this engine cannot do that yet" from "you typed it
/// wrong" without matching on the wording of a sentence. `inillucent-driver`
/// is that caller; `ParseErrorKind::Refused` stays a plain misuse, because it
/// is the reference's own wording for a statement the schema will not have and
/// is not a gap in this engine.
///
/// @param error - the parser's or binder's failure
fn refused(error: inillucent_sql::diagnostic::ParseError) -> inillucent_base::error::DbError {
    // **The sentence goes in the message as well as the detail**, and that is a
    // fix rather than a flourish. `misuse` attaches what it is given as
    // *detail*, so every refusal this engine produced answered `message()` with
    // its primary code's own text - "bad parameter or other API misuse" - and
    // the sentence a person can act on was in the field `inillucent-base`
    // documents as never leaving the process. `inillucent-cli::shell::reason`
    // and `readgate::why` had each worked around it separately, which is what a
    // defect looks like when it has been met twice and fixed neither time.
    //
    // **The primary code comes from `error.code()`, not from `refusal`'s own
    // `SQLITE_MISUSE`.** `ParseError::code` already answers this correctly -
    // `PrimaryCode::Error` for an ordinary compile-time refusal, `TooBig` for
    // the one limit SQLite reports as a parse error - because a parse or bind
    // refusal is `SQLITE_ERROR` in SQLite, not `SQLITE_MISUSE`: `SELECT
    // nosuchcolumn FROM a`, `PRIMARY KEY missing on table x`, `ambiguous
    // column name: v`, `AUTOINCREMENT is only allowed on an INTEGER PRIMARY
    // KEY` and `RAISE() may only be used within a trigger-program` are every
    // one of them code 1 at the reference, measured through
    // `dml_differential.rs`. Routing them all through `refusal` here answered
    // 21 for every one of them - right message, wrong code - which is
    // invisible to a suite that only compares rows and text, and exactly what
    // `dml_differential`'s own primary-code assertions exist to catch.
    //
    // A parse or bind refusal is caller-safe by construction: it names tables,
    // columns and constructs, which are the caller's own words, and never a
    // path, a bound value or page bytes. The detail is left in place so that
    // everything reading it - the shell, the gate, the surface inventory -
    // sees exactly what it saw before.
    let mut built = inillucent_base::error::DbError::primary(error.code())
        .with_message(error.message())
        .with_detail(error.message());
    // **And the position, which used to be dropped here.** A refusal carries the
    // span of the token it is about, and the shell draws the reference's two
    // lines of caret art from it - so losing it here turned every parse failure
    // into a bare sentence where the reference points at the word. A refusal
    // that is deliberately positionless says so with a default span, which is
    // what `no_such_table` and the `ALTER TABLE` refusals use, and those stay
    // positionless because the reference points at nothing for them either.
    if error.span != inillucent_sql::lexer::Span::default() {
        built = built.with_sql_offset(error.offset());
    }
    match error.kind {
        inillucent_sql::diagnostic::ParseErrorKind::Unsupported(what) => {
            built.with_unsupported(what)
        }
        _ => built,
    }
}

/// Names the kind of statement a refusal is about.
///
/// @param statement - the bound statement
fn describe_statement(statement: &BoundStatement) -> &'static str {
    match statement {
        BoundStatement::Select(_) => "a query",
        BoundStatement::Insert(_) => "an insert",
        BoundStatement::Update(_) => "an update",
        BoundStatement::Delete(_) => "a delete",
        BoundStatement::Directive(_) => "a directive",
        BoundStatement::Empty => "nothing",
    }
}

/// The logs one statement writes through, indexed by schema number.
///
/// **Inline for a connection that has one file, which is almost every
/// connection.** A `Vec` here is a heap allocation on every write, and
/// `txn.batched` - two thousand statements inside one transaction - is where
/// that shows: 6.17x to 6.77x across four measured runs, 5.71x to 6.22x with the
/// allocation, on the same fixture, the same rounds and the same machine. It is
/// the only measurable cost the multi-schema write path had, and this is it
/// removed rather than argued away.
enum Logs<'a> {
    /// The only file this connection holds.
    One(WalLog<'a>),
    /// `main`, then `temp`, then the attachments, indexed by schema number.
    Many(Vec<WalLog<'a>>),
}

impl<'a> Logs<'a> {
    /// Returns the log one schema writes through.
    ///
    /// @param at - the schema, as the binder numbers them
    fn get_mut(&mut self, at: usize) -> Option<&mut WalLog<'a>> {
        match self {
            // A connection with one file has one schema, so a handle can only
            // have resolved to `main`; anything else is a plan naming a database
            // that is not there, and the caller refuses it.
            Logs::One(log) => (at == MAIN).then_some(log),
            Logs::Many(held) => held.get_mut(at),
        }
    }

    /// Returns the schemas anything was written through, one bit each.
    fn wrote(&self) -> u16 {
        match self {
            Logs::One(log) => {
                if log.wrote {
                    schema_bit(log.schema)
                } else {
                    0
                }
            }
            Logs::Many(held) => held
                .iter()
                .filter(|log| log.wrote)
                .fold(0, |mask, log| mask | schema_bit(log.schema)),
        }
    }
}

/// The disjoint halves of an [`ImportedDatabase`] a write borrows.
///
/// A write needs `&mut Database` and `&mut PagedTree` at the same instant while
/// the log holds a shared borrow of a third field. Naming the three borrows in
/// one struct is what lets the borrow checker see they are disjoint; a method
/// taking `&mut self` could not, because it would borrow the log too.
struct WriteView<'a> {
    /// The file this connection was opened on.
    database: &'a mut Database,
    /// The files `ATTACH` added beside it.
    attached: &'a mut [Attached],
    /// The temporary databases, one per connection that has one.
    temps: &'a mut [Attached],
    /// The connection this statement belongs to, which is what makes `temp`
    /// mean one of the above rather than another.
    session: u64,
    /// One log per schema, `main`'s first, built once for the statement.
    ///
    /// **One per file, because a statement can write more than one.** A `TEMP`
    /// trigger firing on a write to `main` writes rows into two files, and each
    /// one has to be described in its own log. They are built once per statement
    /// rather than per write, so a statement pays one `Rc` clone per schema
    /// rather than one per row - and none at all beyond the first when the
    /// connection has one file.
    logs: Logs<'a>,
    /// Which schema each tree handle belongs to, for handles that are not
    /// `main`'s.
    owner: &'a HashMap<u32, usize>,
    trees: &'a mut HashMap<u32, PagedTree>,
    layouts: &'a HashMap<u32, std::rc::Rc<SourceLayout>>,
    /// The tables an index a module owns is built over, by root page.
    ///
    /// The write reports what it stored and removed for these and for nothing
    /// else, and the engine applies both to the module afterwards. See
    /// `Changes::written`.
    indexed: &'a HashMap<u32, Vec<VectorIndex>>,
    /// What this statement has written, as
    /// `(rows the statement wrote itself, rows written in all, last rowid)`.
    ///
    /// **The tally that survives a failure.** The `Changes` a write builds is
    /// lost the moment it raises, and `OR FAIL` keeps what it wrote - so
    /// `changes()`, `total_changes()` and `last_insert_rowid()` are read off
    /// the view afterwards on either path. It is a fresh view per statement,
    /// so there is nothing to reset.
    counted: std::cell::Cell<(i64, i64, Option<i64>)>,
    /// Which index trees cover which table, so a query a trigger body runs
    /// inside the write reaches the same covering indexes a typed one does.
    covering: &'a HashMap<u32, Vec<u32>>,
    /// What this connection has registered - `docs/roadmap.md` item 13.
    registry: &'a inillucent_ext::registry::Registry,
}

impl WriteView<'_> {
    /// Returns which schema a tree handle belongs to; `MAIN` when it is
    /// `main`'s.
    ///
    /// @param root - the handle
    fn schema_of(&self, root: u32) -> usize {
        if self.attached.is_empty() && self.temps.is_empty() {
            return MAIN;
        }
        self.owner.get(&root).copied().unwrap_or(MAIN)
    }
}

impl WriteTarget for WriteView<'_> {
    fn count_row(&self, outer: bool) {
        let (own, all, rowid) = self.counted.get();
        self.counted.set((
            own.saturating_add(i64::from(outer)),
            all.saturating_add(1),
            rowid,
        ));
    }

    fn count_rowid(&self, rowid: i64) {
        let (own, all, _) = self.counted.get();
        self.counted.set((own, all, Some(rowid)));
    }

    fn rows_written(&self) -> (i64, i64, Option<i64>) {
        self.counted.get()
    }

    fn parts_for(
        &mut self,
        root: u32,
    ) -> DbResult<(&mut Database, &mut dyn Trees, &mut dyn TreeLog)> {
        let at = self.schema_of(root);
        // Three separate fields of `self`, which is what lets the borrow checker
        // see that the file, the trees and the log are disjoint - the same
        // arrangement this view has always had, one file wider.
        let log = self
            .logs
            .get_mut(at)
            .ok_or_else(|| refusal("a write names a database that is not attached"))?;
        let database = if at == MAIN {
            &mut *self.database
        } else {
            &mut schema_of_index(self.attached, self.temps, self.session, at)
                .ok_or_else(|| refusal("a write names a database that is not attached"))?
                .database
        };
        Ok((database, self.trees, log))
    }

    fn layout(&self, root: u32) -> Option<&std::rc::Rc<SourceLayout>> {
        self.layouts.get(&root)
    }

    fn catalog(&self) -> &dyn TreeCatalog {
        self
    }

    fn captures(&self, root: u32) -> bool {
        self.indexed.contains_key(&root)
    }
}

/// The write's own view of the trees, read as a planned query reads them.
///
/// **The same trees, seen the other way round.** A trigger body is a statement
/// and has to find its rows, and it fires in the middle of a write that is
/// already holding these trees mutably. Answering as a [`TreeCatalog`] as well
/// is what lets `DELETE FROM child WHERE parent_id = OLD.id` reach the ordinary
/// planner - and so the ordinary index probe - rather than a scan written a
/// second time inside the write path.
///
/// A module's rows are the one thing it cannot answer: a virtual table's rows
/// come from the module, the module is registered on the connection, and the
/// connection is exactly what a write has split apart. A trigger body over a
/// virtual table is refused by name rather than answered with nothing.
impl TreeCatalog for WriteView<'_> {
    fn pool_for(&self, root: u32) -> Option<&Pool> {
        let at = self.schema_of(root);
        if at == MAIN {
            return Some(self.database.pool());
        }
        let held = match at {
            TEMP => self
                .temps
                .iter()
                .find(|held| held.session == Some(self.session))?,
            _ => self.attached.get(at.saturating_sub(FIRST_ATTACHED))?,
        };
        Some(held.database.pool())
    }

    fn tree(&self, root: u32) -> Option<&PagedTree> {
        self.trees.get(&root)
    }

    fn layout(&self, root: u32) -> Option<&std::rc::Rc<SourceLayout>> {
        self.layouts.get(&root)
    }

    fn covering_candidates(&self, table_root: u32) -> Vec<u32> {
        self.covering.get(&table_root).cloned().unwrap_or_default()
    }

    fn virtual_cursor(
        &self,
        table: &TableInfo,
        path: &inillucent_sql::plan::AccessPath,
        params: &Params,
        needed: &inillucent_sql::bind::ColumnUse,
        downstream: &mut dyn inillucent_exec::ops::Sink,
    ) -> DbResult<bool> {
        let _ = (path, params, needed, downstream);
        Err(refusal(format!(
            "a trigger body reads {}, which is a virtual table",
            String::from_utf8_lossy(&table.name)
        )))
    }

    // `docs/roadmap.md` item 13.
    fn user_scalar(&self, name: &[u8], argc: usize) -> Option<inillucent_exec::expr::ScalarBody> {
        match self.registry.function(name, argc)?.body.clone() {
            inillucent_ext::registry::UserBody::Scalar(body) => {
                Some(inillucent_exec::expr::ScalarBody(body))
            }
            inillucent_ext::registry::UserBody::Aggregate(_) => None,
        }
    }

    fn user_scalar_is_deterministic(&self, name: &[u8], argc: usize) -> bool {
        self.registry
            .function(name, argc)
            .is_some_and(|function| function.flags.deterministic)
    }
}

/// A [`TreeLog`] that writes to the database's own write-ahead log.
///
/// Every record carries the transaction it belongs to, which is what lets
/// recovery tell a committed change from one whose commit never arrived.
///
/// It holds no handle of its own: when the pool needs to write a page the log
/// has not reached, the pool asks the log directly through the closure
/// `let_the_pool_ask_the_log` registers. See `Pool::on_log_behind`.
struct WalLog<'a> {
    /// The log of the file this one writes into.
    ///
    /// **Owned rather than borrowed.** A write holds its schema's file mutably
    /// while it appends, and a borrow of the log out of the same `Attached`
    /// would be a second borrow of it. An `Rc` clone is a refcount bump, paid
    /// once per schema per statement.
    wal: std::rc::Rc<Wal>,
    txn: u64,
    /// Which schema this log belongs to, as the binder numbers them.
    ///
    /// Stamped onto every before-image, so a rollback puts a row back into the
    /// file it came out of. A tree identifier alone could not say: two files
    /// number their own trees from one.
    schema: usize,
    /// Whether anything has been written through this log.
    ///
    /// **The participant set a cross-file commit needs**, collected where it is
    /// free. A transaction that wrote one file commits the way it always did; a
    /// transaction that wrote two is decided by a super-journal, and this is how
    /// the commit knows which it is.
    wrote: bool,
    /// Where before-images go while a transaction is open, or `None` outside
    /// one.
    ///
    /// An autocommit statement cannot be abandoned, so it collects nothing and
    /// pays nothing for the possibility. The buffer is handed in by the caller
    /// rather than owned here because it has to outlive the log: the log lives
    /// for one statement and the transaction for many.
    undo: Option<&'a std::cell::RefCell<Vec<Before>>>,
    /// This schema's own no-steal watermark - `u64::MAX` until something is
    /// open, or the open transaction's first record.
    ///
    /// Shared with the schema's `Pool`, which is the only other reader:
    /// arming it here is the one place that knows which record was first, and
    /// `Pool::writeback` is the one place that must not write a page stamped
    /// at or above it. See `Pool::holds_uncommitted`.
    uncommitted: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl TreeLog for WalLog<'_> {
    fn log(&mut self, body: Body<'_>) -> DbResult<u64> {
        self.wrote = true;
        let lsn = self.wal.append(self.txn, body)?;
        // Armed once, on the first record since the last commit or rollback -
        // never rearmed while it is already set, so a later record in the
        // same transaction (an ordinary write, or an undo's own restore)
        // cannot move the watermark past the point recovery must not pass.
        if self.uncommitted.load(std::sync::atomic::Ordering::SeqCst) == u64::MAX {
            self.uncommitted
                .store(lsn, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(lsn)
    }

    fn wants_undo(&self) -> bool {
        self.undo.is_some()
    }

    fn undo(
        &mut self,
        tree: u64,
        key: &[Datum<'_>],
        before: Option<Vec<OwnedDatum>>,
    ) -> DbResult<()> {
        if let Some(buffer) = self.undo {
            // **The key is copied only when there is no row to put back.** A
            // restore that has a row calls `put`, which reads the key columns
            // out of the row itself; copying them a second time allocated a
            // vector per write and threw it away on every update and delete.
            let key = match before {
                Some(_) => Vec::new(),
                None => key.iter().map(OwnedDatum::from_datum).collect(),
            };
            buffer.borrow_mut().push(Before {
                schema: self.schema,
                tree,
                key,
                row: before,
            });
        }
        Ok(())
    }
}

/// One row as it was before a statement inside a transaction changed it.
///
/// **Not `inillucent_txn::Undo`, and the difference is worth stating.** That
/// type carries a key and a before-image as `Vec<u8>`, because the transaction
/// engine below works in encoded rows. This engine's rows are `OwnedDatum`
/// vectors all the way down - the write path takes them, the trees store them
/// as PAX mini-columns, and there is no row-bytes encoding to borrow. Encoding
/// a row to bytes to record it and decoding it to restore it would be inventing
/// a third representation to bridge two that already exist.
///
/// So the two undo buffers are not duplicates of each other; they are the same
/// idea at two layers that disagree about what a row is, and that disagreement
/// is why routing this engine's writes through `inillucent_txn::Transaction` is
/// a piece of work rather than a wiring job.
#[derive(Clone, Debug)]
struct Before {
    /// Which schema the tree is in, as the binder numbers them.
    ///
    /// **Because a tree identifier is a file's number, not a connection's.**
    /// Two databases each number their own trees from one, so an undo record
    /// naming tree 7 says nothing until it also says which file - and a rollback
    /// that guessed would restore a row into the wrong database.
    schema: usize,
    /// The tree the row is in, by the identifier its own file knows it by.
    tree: u64,
    /// The row's key columns, and empty when `row` carries them.
    ///
    /// A restore with a row to write back finds the key inside it, so the copy
    /// is made only for the case that needs one: a key that was not there, put
    /// back by deleting it again.
    key: Vec<OwnedDatum>,
    /// The whole row as it was, or `None` when the key was not there.
    row: Option<Vec<OwnedDatum>>,
}

/// Puts imported rows into the order the tree they are about to build compares
/// in.
///
/// **The import cannot rely on SQLite's physical order being ours.** It reads a
/// b-tree by walking it, so the rows arrive in the order *that* file kept them,
/// and there are two ways for that to differ from the order the new tree
/// defines. A `DESC` index column is stored descending by SQLite and ascending
/// here. A collated column is stored under SQLite's implementation of the
/// collation, and agreeing with it byte for byte is an assumption rather than a
/// fact.
///
/// A tree whose leaves are not in its own key order answers a **scan** exactly
/// right and a **seek** wrongly, because the descent binary-searches separators
/// it does not actually obey. That is why this was invisible until the write
/// path became the first thing to seek into an index: `members_score`, over
/// `(score DESC, email)`, imported out of order, and every delete against it
/// silently found nothing and left the entry behind.
///
/// The sort key is the tree's *own* encoding under the tree's *own* collations,
/// so there is no second opinion about ordering to drift from the first.
///
/// @param rows - the rows as the file gave them up
/// @param columns - the tree's column directory
/// @param key_columns - how many leading columns form the key
fn in_key_order(
    rows: Vec<Vec<OwnedDatum>>,
    columns: &[ColumnSpec],
    key_columns: usize,
) -> Vec<Vec<OwnedDatum>> {
    let collations: Vec<Collation> = columns
        .iter()
        .take(key_columns)
        .map(|spec| spec.collation)
        .collect();
    // And the directions, because "key order" is the *tree's* order and a
    // descending key column is part of what that order is. Sorting ascending
    // and then building a tree whose comparisons are descending produces a tree
    // that is sorted by nothing either half agrees with.
    let directions: Vec<bool> = columns
        .iter()
        .take(key_columns)
        .map(|spec| spec.descending)
        .collect();
    // **Sorted by comparing the values, not by encoding a key per row.**
    //
    // The version this replaces built a `Vec<u8>` key for every row, sorted the
    // pairs by memcmp and then rebuilt the vector - two moves of every row and
    // one allocation per row, to reproduce an order the values already have.
    // `compare_rows` is the comparison the tree's own search and its integrity
    // checker use, so sorting by it is what the tree will be read by, and the
    // encoded form is derived from the same order rather than defining it.
    //
    // It is a stable sort because a `sort_unstable` here would reorder rows
    // whose whole key is equal, and a bulk build's input is compared against
    // SQLite's index page for page.
    let mut rows = rows;
    rows.sort_by(|left, right| {
        for column in 0..key_columns {
            let (Some(one), Some(two)) = (left.get(column), right.get(column)) else {
                continue;
            };
            let order = inillucent_tree::types::compare_under(
                &one.borrow(),
                &two.borrow(),
                collations.get(column).copied().unwrap_or(Collation::Binary),
            );
            let order = if directions.get(column).copied().unwrap_or(false) {
                order.reverse()
            } else {
                order
            };
            if order != std::cmp::Ordering::Equal {
                return order;
            }
        }
        std::cmp::Ordering::Equal
    });
    rows
}

/// Returns a built tree's shape as the catalog records it.
///
/// @param shape - what the build produced
fn stats_of(shape: &TreeShape) -> inillucent_catalog::paged::TreeStats {
    inillucent_catalog::paged::TreeStats {
        first_leaf: shape.first_leaf,
        leaf_count: shape.leaf_count,
        row_count: shape.row_count,
    }
}

/// One catalog row, with the identifier of the tree it describes.
///
/// The row is what the file holds; the identifier is what the `trees` and
/// `layouts` maps are keyed by. They are different numbers - see the module
/// documentation on `newengine::ddl` - and carrying them together is what lets a
/// rename change the row without the tree it names moving.
#[derive(Clone, Debug)]
struct Recorded {
    /// The rowid the catalog tree stores it under.
    rowid: i64,
    /// The identifier its tree is registered under, zero when it has no tree.
    root: u32,
    /// The row itself.
    entry: SchemaEntry,
}

/// The shape of one built tree, kept so it can be re-attached after the file is
/// closed and reopened.
///
/// It is what the catalog will hold in Phase 3. Carrying it explicitly rather
/// than rediscovering it on open is deliberate: rediscovering a root's height
/// by reading the root is fine, but rediscovering its *row count* means walking
/// it, and a harness that walked every tree on open would be measuring its own
/// startup.
struct TreeShape {
    root: PageId,
    columns: Vec<ColumnSpec>,
    key_columns: usize,
    first_leaf: PageId,
    leaf_count: u64,
    row_count: u64,
}

/// The root number `sqlite_schema` is registered under.
///
/// A root number is only an identifier here - the physical page comes from the
/// meta record - so the catalog takes one no imported table can be given. SQLite
/// roots start at 1 and count pages, so a number at the top of the range is
/// free by construction.
const SCHEMA_VIEW_ROOT: u32 = u32::MAX;

/// The identifier the first DDL-created tree is registered under.
///
/// Imported trees are keyed by the fixture's SQLite root *page*, which counts
/// pages from one, so a fixture would have to be eight terabytes at the default
/// page size before it reached this. Counting up from here keeps every created
/// tree's identifier distinct from every imported one without a search.
const FIRST_CREATED_ROOT: u32 = 0x8000_0000;

/// The first handle a tree of an attached database is registered under.
///
/// **A handle is the connection's name for a tree; a tree identifier is the
/// file's.** They are the same number for `main` and they cannot be for anything
/// else: two files number their own trees from one, so a connection holding both
/// would have two trees under one key. So `main` keeps identity and every other
/// schema's trees are re-numbered into the range above this.
///
/// The two ranges cannot meet by growth, because `allocate_root` refuses at this
/// number: a `main` holding 2^30 created objects is refused by name rather than
/// silently handed a handle an attached database already answers to.
const FIRST_ATTACHED_HANDLE: u32 = 0xC000_0000;

/// How many databases a connection may hold beside `main` and `temp`.
///
/// SQLite's `SQLITE_MAX_ATTACHED` default, and the number `attach.rs` grades
/// against.
const MAX_ATTACHED: usize = 10;

/// Returns the path the imported database is written to.
///
/// The page size and the frame count are in the name so that a sweep over
/// either does not overwrite the previous run's file while it is still open.
///
/// @param fixture - the SQLite fixture being imported
/// @param page_size - the page size the trees are built at
/// @param frames - how many frames the pool holds
fn target_path(fixture: &std::path::Path, page_size: usize, frames: usize) -> PathBuf {
    let stem = fixture
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "fixture".to_string());
    let directory = fixture
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    directory.join(format!("{stem}-p{page_size}-f{frames}.rdb"))
}

/// Tells a pool how to make the log catch up when it is behind.
///
/// **Registered wherever a database and a log come together**, which is the
/// import, the open and the reopen. Without it the pool can only refuse a page
/// whose LSN is past the durable point, and a statement that dirties more pages
/// than the pool holds has no way to satisfy it: `CREATE INDEX` on the large
/// fixture failed on every one of thirty qualification rounds for exactly that
/// reason.
///
/// The sync is real. `write_ahead_point` is `durable_end` under NORMAL and
/// FULL, so what is handed back is a point the log has reached rather than one
/// it has merely been given bytes for, and the pool's guard still refuses if it
/// is not far enough.
///
/// @param pool - the pool that will do the asking
/// @param wal - the log it should ask
fn let_the_pool_ask_the_log(pool: &Pool, wal: &std::rc::Rc<Wal>) {
    let held = std::rc::Rc::clone(wal);
    pool.on_log_behind(std::rc::Rc::new(move || {
        held.sync()?;
        Ok(held.write_ahead_point())
    }));
}

/// Returns `sqlite_schema` under the name almost every tool actually types.
///
/// **`sqlite_master` is the same table, and a database that could not answer it
/// would be one no existing tool could inspect.** SQLite accepts both names;
/// the old engine synthesised the alias in `inillucent-catalog`, through a
/// helper that reaches into `inillucent-storage` and so cannot outlive it. This
/// is the same idea with the new engine's own schema table, registered beside
/// it rather than instead of it.
///
/// The alias is a name that resolves, not a row: `sqlite_schema` has never
/// listed itself, and it does not list this either.
///
/// @param schema - the schema table's own declaration
fn schema_alias_of(schema: &TableInfo) -> TableInfo {
    schema_named(schema, b"sqlite_master")
}

/// Returns one schema's catalog declaration under another name.
///
/// A temporary database's catalog is `sqlite_temp_schema` and
/// `sqlite_temp_master`; an attached one's is `sqlite_schema` and
/// `sqlite_master` under its own qualifier. Same tree, same five columns, same
/// handle - only the name a statement writes differs.
///
/// @param schema - the catalog declaration to rename
/// @param name - the name it will answer to
fn schema_named(schema: &TableInfo, name: &[u8]) -> TableInfo {
    let mut alias = schema.clone();
    alias.name = name.to_vec();
    alias.folded = name.to_ascii_lowercase();
    alias
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

/// Returns whether a table is one a virtual table owns.
///
/// FTS5 keeps `<name>_data`, `<name>_idx`, `<name>_docsize`, `<name>_content`
/// and `<name>_config`; R*Tree keeps `<name>_node`, `<name>_rowid` and
/// `<name>_parent`. Recognised by the prefix rather than by a list of suffixes,
/// because the suffixes are the module's to choose and a list would be right
/// until a module added one.
///
/// @param owners - the folded names of the file's virtual tables
/// @param folded - the folded name of the table being considered
fn is_shadow_of(owners: &[Vec<u8>], folded: &[u8]) -> bool {
    owners.iter().any(|owner| {
        folded
            .strip_prefix(owner.as_slice())
            .and_then(|rest| rest.strip_prefix(b"_".as_slice()))
            .is_some_and(|suffix| !suffix.is_empty())
    })
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
        Cached::Insert(..) | Cached::VirtualInsert(_) | Cached::Update(..) | Cached::Delete(..) => {
            true
        }
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

/// Returns whether a declared column is a `VIRTUAL` generated one.
///
/// The one predicate behind the whole `VIRTUAL` shift. A `VIRTUAL` generated
/// column is computed on read and never written, so it occupies no field in a
/// SQLite record and no column in one of this engine's trees; a `STORED` one is
/// an ordinary column that happens to have been filled in by an expression.
///
/// @param info - the table's declaration
/// @param declared - the column's declared position
fn is_virtual_column(info: &TableInfo, declared: usize) -> bool {
    info.columns
        .get(declared)
        .is_some_and(|column| column.generated && !column.stored)
}

/// Returns the declared positions a table's record holds, in record order.
///
/// **One derivation of "which columns are actually stored", used by the shape,
/// the import and the row comparison alike.** Everything that walks a record -
/// the tree builder, the fixture importer, `logical_row` - used to walk
/// `0..info.columns.len()` and so silently assumed that a declared position and
/// a record field are the same number. They are, until a table declares a
/// `VIRTUAL` generated column, after which every later column reads one field
/// early and returns its neighbour's value: data rather than a refusal, which
/// is the one failure this engine is not allowed to have.
///
/// The rowid-alias column is included, because SQLite's record does carry a
/// (NULL) field for it and the callers drop it themselves.
///
/// @param info - the table's declaration
fn stored_positions(info: &TableInfo) -> Vec<usize> {
    (0..info.columns.len())
        .filter(|declared| !is_virtual_column(info, *declared))
        .collect()
}

/// Returns the journal a connection in `mode` needs to protect a checkpoint.
///
/// **A write-ahead log does not remove the need for a rollback journal here,
/// and that is a consequence of the log being logical.** A checkpoint writes
/// pages into the data file in place. Once a page's content is below the
/// recorded checkpoint point, the log no longer describes it: the records that
/// built it have been made redundant and their segments retired. So a page the
/// checkpoint half wrote before a power loss is content nothing can rebuild -
/// not the log, which has moved past it, and not the page itself, which is
/// torn. SQLite is not exposed to this because its log holds whole page images
/// and a checkpoint is a copy, so an interrupted one is simply redone.
///
/// `crates/inillucent-compat/tests/wal_crash.rs`'s checkpoint campaign found
/// it: a crash inside `PRAGMA wal_checkpoint` left pages 2 and 3 written and
/// unsynced, the meta record correctly still naming the *previous* checkpoint,
/// and recovery unable to read a page it could not rebuild either -
/// `page 3 checksum fe9063aa is not the computed f53956bb`.
///
/// So a connection in `wal` takes a `delete` journal, which holds the
/// pre-images for the duration of a checkpoint and removes the file when the
/// checkpoint's meta record is durable. The cost is the one the default mode
/// already pays; what it buys is that an interrupted checkpoint is undoable in
/// every mode rather than in three of the five.
///
/// `off` is the one mode that gets nothing, because that is what it asks for.
///
/// @param mode - what `PRAGMA journal_mode` reports
fn journal_for(
    mode: inillucent_pool::journal::JournalMode,
) -> Option<inillucent_pool::journal::JournalMode> {
    match mode {
        inillucent_pool::journal::JournalMode::Off => None,
        inillucent_pool::journal::JournalMode::Wal => {
            Some(inillucent_pool::journal::JournalMode::Delete)
        }
        other => Some(other),
    }
}
