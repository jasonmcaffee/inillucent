//! The rearchitected engine as a database.
//!
//! Invariant: **this crate is the engine, and nothing above it is.** It owns
//! the buffer pool, the trees, the log, the catalog, DDL, the pragma set, the
//! statement path and the virtual-table host, and it depends on none of the
//! old engine - not `inillucent-storage`, not `inillucent-transaction`, not
//! `inillucent-vm`. A caller reaches the new engine by depending on this and
//! on nothing else.
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
#![deny(clippy::indexing_slicing)]
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
pub mod connect;
pub mod ddl;
mod entries;
pub(crate) mod import;
mod inspect;
mod introspect;
pub mod multi;
mod plans;
pub mod pragma;
mod rebuild;
pub mod vtab;

use std::collections::HashMap;
use std::path::PathBuf;

use inillucent_base::error::{refusal, Unwind};
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
    attach_catalog, read_catalog, schema_create_sql, schema_layout, write_catalog, ObjectKind,
    SchemaEntry,
};
use inillucent_exec::dml::{self, Changes, Trees, WriteTarget};
use inillucent_exec::physical::{self, ForcePlan, Params, SourceLayout, TreeCatalog};
use inillucent_exec::StaticType;
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
use inillucent_sql::catalog_view::{IndexInfo, StaticCatalog, TableInfo};
use inillucent_sql::parser::parse_next_statement;
use inillucent_sql::plan::{plan_select_with, Levers, PhysicalPlan};
use inillucent_sqlite_reader::SqliteFile;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::types::{ColumnSpec, PhysicalType};
use inillucent_tree::write::TreeLog;
use inillucent_tree::PagedTree;
use inillucent_txn::redo::{RowRedo, TreeRows};
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
    index_stages: std::cell::Cell<(u128, u128, u128, u128, u128, u128, u128)>,
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
    pub fn open_session(&self) -> u64 {
        let session = self.next_session.get();
        self.next_session.set(session.saturating_add(1));
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
    fn module_integrity(&self, name: &[u8]) -> DbResult<Option<Option<String>>> {
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

impl ImportedDatabase {
    /// Imports a SQLite fixture into PAX trees in a fresh database file.
    ///
    /// @param path - the SQLite database to read
    /// @param page_size - the page size to build the new trees at
    pub fn import(path: PathBuf, page_size: usize) -> DbResult<ImportedDatabase> {
        ImportedDatabase::import_with(path, page_size, DEFAULT_FRAMES)
    }

    /// Imports a SQLite fixture, with the pool size stated.
    ///
    /// The file is written beside the fixture with a `.rdb` suffix, built,
    /// checkpointed, closed and reopened - so what the measurement reads is a
    /// database that came off a disk, through a pool of the size named here,
    /// rather than a tree that never left memory. That distinction is the whole
    /// of the Phase 1 report's outstanding fairness question.
    ///
    /// @param path - the SQLite database to read
    /// @param page_size - the page size to build the new trees at
    /// @param frames - how many frames the buffer pool holds
    pub fn import_with(
        path: PathBuf,
        page_size: usize,
        frames: usize,
    ) -> DbResult<ImportedDatabase> {
        let target = target_path(&path, page_size, frames);
        ImportedDatabase::import_into(path, target, page_size, frames)
    }

    /// Imports a SQLite file into a target the caller names.
    ///
    /// The same import, with the destination stated rather than derived. A
    /// measurement wants the derived name - the page size and frame count are
    /// in it so a sweep does not overwrite a file it still has open - and a
    /// **migration** wants to choose, because it stages into a uniquely named
    /// file beside the destination and publishes by renaming. A half-written
    /// database must never sit at the path an application opens, and that is a
    /// property of *where* it is written.
    ///
    /// The target is removed first if it exists, so a caller that stages into a
    /// fresh name gets a fresh file and one that reuses a name gets a rebuild
    /// rather than a merge.
    ///
    /// @param path - the SQLite database to read
    /// @param target - the file to build
    /// @param page_size - the page size to build the new trees at
    /// @param frames - how many frames the buffer pool holds
    pub fn import_into(
        path: PathBuf,
        target: PathBuf,
        page_size: usize,
        frames: usize,
    ) -> DbResult<ImportedDatabase> {
        // **The seven phases, in the one order that works.** The reopen is what
        // the order exists for: everything after it reads a file this process
        // did not write, so the format round-trip is checked rather than
        // assumed. Each phase is in `crate::import` with its own argument.
        let mut source = crate::import::read_source(path)?;
        let (vfs, db_path, mut database) =
            crate::import::create_destination(&target, page_size, frames)?;
        let mut carried = crate::import::Carried::new();
        crate::import::carry_tables(&mut database, &mut source, &mut carried)?;
        crate::import::carry_treeless(&source, &mut carried);
        let catalog_shape = crate::import::write_schema(&mut database, &carried)?;
        let database =
            crate::import::reopen_and_verify(database, &vfs, &db_path, frames, &carried)?;

        let mut trees = HashMap::new();
        for (root, shape) in &carried.shapes {
            let tree = PagedTree::attach(
                database.pool(),
                u64::from(*root),
                shape.root,
                shape.columns.clone(),
                shape.key_columns,
                shape.leaf_count,
                shape.row_count,
            )?;
            trees.insert(*root, tree);
        }

        // `sqlite_schema` joins the catalog as a table like any other, keyed by
        // a root number no imported table can have. `SELECT ... FROM
        // sqlite_schema` is then planned, scanned and projected by the ordinary
        // path - there is no view machinery, because there does not need to be.
        let schema_root = SCHEMA_VIEW_ROOT;
        let schema_info = table_from_create_sql(schema_create_sql(), 0, schema_root)?;
        carried.layouts.insert(
            schema_root,
            std::rc::Rc::new(SourceLayout {
                tree_key: schema_root,
                // The rowid is the tree's key column and is not one of the five
                // declared columns, so the record slots start at tree column 1.
                slots: (1..=5).map(Some).collect(),
                rowid: Some(0),
                identity: vec![0],
                types: vec![
                    StaticType::Int,
                    StaticType::Text,
                    StaticType::Text,
                    StaticType::Text,
                    StaticType::Int,
                    StaticType::Text,
                ],
                width: 6,
                key_columns: vec![0],
            }),
        );
        trees.insert(
            schema_root,
            PagedTree::attach(
                database.pool(),
                u64::from(schema_root),
                catalog_shape.root,
                catalog_shape.columns.clone(),
                catalog_shape.key_columns,
                catalog_shape.leaf_count,
                catalog_shape.row_count,
            )?,
        );
        carried.catalog = carried.catalog.with_table(schema_info.clone());
        carried.catalog = carried.catalog.with_table(schema_alias_of(&schema_info));

        // The log the write path describes every change in, opened on a file
        // that has just been checkpointed - so it starts empty, at the first
        // stream position, and every record in it is one this process wrote.
        let wal = std::rc::Rc::new(Wal::open(
            std::sync::Arc::clone(&vfs),
            &db_path,
            database.uuid(),
            FIRST_LSN,
            1,
            WalOptions::default(),
        )?);
        database.pool().set_durable_lsn(wal.write_ahead_point());
        let_the_pool_ask_the_log(database.pool(), &wal);

        // Smallest tree first, so the physical pass takes the cheapest
        // structure that covers the query. Sorting by bytes rather than by
        // column count is what makes it the right order: an index with more
        // columns but shorter values can still be the smaller scan.
        for roots in carried.covering.values_mut() {
            roots.sort_by_key(|root| {
                trees
                    .get(root)
                    .map(PagedTree::byte_size)
                    .unwrap_or(usize::MAX)
            });
        }

        let mut opened = ImportedDatabase {
            catalog: carried.catalog,
            database,
            trees,
            layouts: carried.layouts,
            covering: carried.covering,
            page_size,
            frames,
            path: target,
            skipped: carried.skipped,
            limits: Limits::default(),
            wal,
            next_txn: std::cell::Cell::new(1),
            last_rowid: std::cell::Cell::new(0),
            last_changes: std::cell::Cell::new(0),
            seed: std::cell::Cell::new(fresh_seed()),
            changed_ever: std::cell::Cell::new(0),
            statements: std::cell::RefCell::new(HashMap::new()),
            compiles: std::cell::Cell::new(0),
            batch: std::cell::Cell::new(None),
            undo: std::cell::RefCell::new(Vec::new()),
            touched: 0,
            decided_over: std::cell::Cell::new(0),
            marks: Vec::new(),
            entries: carried
                .entries
                .into_iter()
                .zip(carried.identifiers)
                .enumerate()
                .map(|(nth, (entry, root))| Recorded {
                    rowid: nth.saturating_add(1) as i64,
                    root,
                    entry,
                })
                .collect(),
            tables: carried.tables,
            schema_info,
            next_root: FIRST_CREATED_ROOT,
            busy_timeout_ms: 0,
            foreign_keys: false,
            defer_foreign_keys: false,
            // **SQLite's own two defaults, and each one is a measurement.**
            //
            // `delete` because it costs nothing: the medium gate reads 3.78x
            // weighted with `wal` and 3.70x with `delete`, lower bounds 3.45x
            // and 3.44x - the same number twice, over 30 paired rounds on one
            // machine. The write-ahead log is still here and
            // `PRAGMA journal_mode = WAL` still switches to it; what changed is
            // which one a caller gets without asking, and the answer is now the
            // one every other SQLite gives.
            //
            // `exclusive` because it does not. The same gate with
            // `locking_mode = normal` as the default reads **3.03x with a lower
            // bound of 2.95x - under the contract's 3.00x bar** - and takes
            // `write` from 1.94x to 1.19x, `transaction` from 0.89x to 0.37x
            // and `schema` from 1.34x to 0.66x, because releasing the file
            // between statements means re-reading the meta record before each
            // one. `PRAGMA locking_mode = normal` is a real switch and a second
            // process can then open the file; making it the default would pay
            // for that on every statement of every program that never opens a
            // second connection.
            journal_mode: inillucent_pool::journal::JournalMode::Delete,
            running: 0,
            locking_exclusive: true,
            ignore_check_constraints: false,
            secure_delete: 0,
            auto_vacuum: 0,
            automatic_index: true,
            settling: std::cell::Cell::new(false),
            scratch_ast: std::cell::RefCell::new(None),
            levers: Levers::default(),
            case_sensitive_like: false,
            cache_size: None,
            analysis_limit: 0,
            writable_schema: false,
            defensive: false,
            imposters: Vec::new(),
            authorizer: None,
            vfs: std::sync::Arc::clone(&vfs),
            query_only: false,
            recursive_triggers: false,
            max_page_count: crate::pragma::DEFAULT_MAX_PAGE_COUNT,
            temp_store: 0,
            collations: Vec::new(),
            registry: modules(),
            eponymous: Vec::new(),
            virtual_tables: HashMap::new(),
            vector_indexes: HashMap::new(),
            index_stages: std::cell::Cell::new((0, 0, 0, 0, 0, 0, 0)),
            catalog_generation: 0,
            attached: Vec::new(),
            temps: Vec::new(),
            session: std::cell::Cell::new(0),
            next_session: std::cell::Cell::new(1),
            tables_session: 0,
            owner: HashMap::new(),
            next_handle: FIRST_ATTACHED_HANDLE,
            ddl_schema: 0,
        };
        opened.settle_journal()?;
        Ok(opened)
    }

    /// Reconciles the journal mode with what the file says, and installs it.
    ///
    /// **Two things a constructor cannot do for itself.** The mode a connection
    /// starts in is not a constant: a database left in WAL comes back in WAL,
    /// because the alternative is one connection writing pre-images beside a
    /// log another is appending frames to. And a rollback mode needs its
    /// `Journal` attached to the pool - without it the mode is a word the
    /// pragma reports and nothing writes a pre-image, which is a durability
    /// hole rather than a cosmetic one.
    fn settle_journal(&mut self) -> DbResult<()> {
        let mode = if self.database.wal_mode() {
            inillucent_pool::journal::JournalMode::Wal
        } else {
            self.journal_mode
        };
        self.journal_mode = mode;
        let held: std::sync::Arc<dyn inillucent_vfs::Vfs> = std::sync::Arc::clone(&self.vfs);
        let journal = mode.is_rollback().then(|| {
            inillucent_pool::journal::Journal::new(
                held,
                &DbPath::new(self.path.to_string_lossy().as_ref()),
                mode,
                self.page_size,
            )
        });
        self.database.pool().set_journal(journal);
        Ok(())
    }

    /// Creates a fresh, empty database.
    ///
    /// **The primitive the re-rooting needs, and the one the engine did not
    /// have.** It could `import` a SQLite file and, since this ticket, `open` a
    /// file it had written; it could not make one. `Database::open` on a path
    /// that does not exist has to create it, so a connection cannot be re-rooted
    /// onto this engine without it.
    ///
    /// What it writes is the smallest legal database: the file, its meta page,
    /// and a catalog tree with no rows in it. Everything else - tables, indexes,
    /// virtual tables - arrives through DDL afterwards, which is the path that
    /// already exists and is already tested.
    ///
    /// It goes through the same close-and-reopen the import does, for the same
    /// reason: what the caller gets back has been read off a disk rather than
    /// kept in the pool that wrote it, so a format that does not round-trip
    /// fails here rather than in a query much later.
    ///
    /// @param path - where to create the database
    /// @param page_size - the page size to build at
    /// @param frames - how many frames the buffer pool holds
    pub fn create(path: PathBuf, page_size: usize, frames: usize) -> DbResult<ImportedDatabase> {
        ImportedDatabase::create_on(std::sync::Arc::new(OsVfs::new()), path, page_size, frames)
    }

    /// Creates a fresh, empty database on a file system of the caller's.
    ///
    /// The general form of [`ImportedDatabase::create`], and what `:memory:`
    /// goes through: a `MemoryVfs` given here is the file system the database,
    /// its log and its journal all live on, and it disappears with the last
    /// handle to it.
    ///
    /// @param vfs - the file system to build on
    /// @param path - where to create the database
    /// @param page_size - the page size to build at
    /// @param frames - how many frames the buffer pool holds
    pub fn create_on(
        vfs: std::sync::Arc<dyn inillucent_vfs::Vfs>,
        path: PathBuf,
        page_size: usize,
        frames: usize,
    ) -> DbResult<ImportedDatabase> {
        let db_path = DbPath::new(path.to_string_lossy().as_ref());
        let _ = vfs.delete(&db_path, false);
        let mut database = Database::create(
            vfs.as_ref(),
            &db_path,
            Options::default()
                .with_page_size(page_size)
                .with_frames(frames.max(64)),
        )?;
        // An empty catalog is a catalog tree with no entries, not the absence of
        // one: every later DDL statement inserts into it, and a database whose
        // catalog root pointed nowhere would be one no `CREATE TABLE` could
        // start from.
        let _ = write_catalog(&mut database, &[])?;
        database.checkpoint()?;
        drop(database);
        ImportedDatabase::open_on(vfs, path, page_size, frames)
    }

    /// Opens a database this engine wrote, reading its schema from the file.
    ///
    /// **This is the engine's own open path, and it is a different thing from
    /// [`ImportedDatabase::reopen`].** `reopen` closes and reopens a handle this
    /// process already has, and it carries that handle's column specifications
    /// and tree numbering across - which is correct for what it is for, proving
    /// that a checkpointed file reads back, but it cannot open a file this
    /// process did not write. Its own comment says so: "a genuine open would
    /// number them itself" and "the engine's own open path is Phase 5's consumer
    /// story".
    ///
    /// This is that path. Nothing comes from memory: the catalog tree is
    /// attached from the meta page's root, every row is read out of it, and each
    /// object's shape is derived from the `CREATE` text the row carries, by the
    /// same `table_shape` / `keyed_table_shape` / `index_shape` the import uses.
    /// One derivation, so a file that opens differently from the way it was
    /// built is a bug in one function rather than a disagreement between two.
    ///
    /// The per-tree statistics come from the catalog row rather than from a walk
    /// - which is what `TreeStats` is in the file for, and what makes opening a
    /// large database cost a catalog read instead of a scan of every leaf.
    ///
    /// @param path - the database file to open
    /// @param page_size - the page size the file was built at
    /// @param frames - how many frames the buffer pool holds
    pub fn open(path: PathBuf, page_size: usize, frames: usize) -> DbResult<ImportedDatabase> {
        ImportedDatabase::open_on(std::sync::Arc::new(OsVfs::new()), path, page_size, frames)
    }

    /// Opens a database on a file system of the caller's.
    ///
    /// The general form of [`ImportedDatabase::open`]; see
    /// [`ImportedDatabase::create_on`] for why the file system is held rather
    /// than made where it is used.
    ///
    /// @param vfs - the file system the database lives on
    /// @param path - the database file to open
    /// @param page_size - the page size the file was built at
    /// @param frames - how many frames the buffer pool holds
    pub fn open_on(
        vfs: std::sync::Arc<dyn inillucent_vfs::Vfs>,
        path: PathBuf,
        page_size: usize,
        frames: usize,
    ) -> DbResult<ImportedDatabase> {
        let db_path = DbPath::new(path.to_string_lossy().as_ref());
        // **A file opened as `main` asks the same question an attached one
        // does.** A database this connection is opened on may have been the
        // participant of a cross-file commit that a crash caught undecided, and
        // there is nothing about being `main` that settles it.
        // **A hot rollback journal is replayed before anything reads a page.**
        // It describes a file that is halfway through a transaction, and every
        // page it names has to go back before the meta record is even read -
        // the meta page itself may be one of them. A journal whose header is
        // absent or zeroed describes nothing and is removed, which is what a
        // finished one looks like. See `inillucent_pool::journal`.
        inillucent_pool::journal::replay_hot_journal(vfs.as_ref(), &db_path)?;
        let doubtful = multi::doubtful_transactions(&path)?;
        let opened_file = open_file(&vfs, &db_path, frames, &doubtful)?;
        let OpenedFile {
            database,
            wal,
            catalog_tree,
            highest_txn,
        } = opened_file;
        // **One derivation for every file this connection can name.** The
        // shapes are read out of the catalog tree by `load_schema`, which is
        // the same function `ATTACH` uses - so a file opened as `main` and the
        // same file attached as `aux` are planned against identical
        // declarations rather than against two derivations that agree today.
        //
        // `main`'s handles are its own local identifiers, unchanged: the
        // database a connection was opened on keeps the numbering it has always
        // had, which is what makes a one-file connection the code it was.
        let loaded = load_schema(
            &database,
            catalog_tree,
            0,
            b"main",
            SCHEMA_VIEW_ROOT,
            &mut |local| local,
        )?;
        let LoadedSchema {
            trees,
            layouts,
            covering,
            entries,
            tables: loaded_tables,
            schema_info,
            handles: _main_handles,
            skipped,
            highest_identifier,
        } = loaded;
        let mut catalog = StaticCatalog::empty();
        for info in &loaded_tables {
            catalog = catalog.with_table(info.clone());
        }
        catalog = catalog.with_table(schema_info.clone());
        catalog = catalog.with_table(schema_alias_of(&schema_info));

        let mut opened = ImportedDatabase {
            catalog,
            database,
            trees,
            layouts,
            covering,
            page_size,
            frames,
            path,
            skipped,
            limits: Limits::default(),
            wal,
            // Above every number the log still holds, so that this run cannot
            // call something by a name a crashed one already used.
            next_txn: std::cell::Cell::new(highest_txn.saturating_add(1)),
            last_rowid: std::cell::Cell::new(0),
            last_changes: std::cell::Cell::new(0),
            seed: std::cell::Cell::new(fresh_seed()),
            changed_ever: std::cell::Cell::new(0),
            statements: std::cell::RefCell::new(HashMap::new()),
            compiles: std::cell::Cell::new(0),
            batch: std::cell::Cell::new(None),
            undo: std::cell::RefCell::new(Vec::new()),
            touched: 0,
            decided_over: std::cell::Cell::new(0),
            marks: Vec::new(),
            entries,
            tables: Vec::new(),
            schema_info,
            next_root: highest_identifier.saturating_add(1).max(FIRST_CREATED_ROOT),
            attached: Vec::new(),
            temps: Vec::new(),
            session: std::cell::Cell::new(0),
            next_session: std::cell::Cell::new(1),
            tables_session: 0,
            owner: HashMap::new(),
            next_handle: FIRST_ATTACHED_HANDLE,
            ddl_schema: 0,
            busy_timeout_ms: 0,
            foreign_keys: false,
            defer_foreign_keys: false,
            journal_mode: inillucent_pool::journal::JournalMode::Delete,
            running: 0,
            locking_exclusive: true,
            ignore_check_constraints: false,
            secure_delete: 0,
            auto_vacuum: 0,
            automatic_index: true,
            settling: std::cell::Cell::new(false),
            scratch_ast: std::cell::RefCell::new(None),
            levers: Levers::default(),
            case_sensitive_like: false,
            cache_size: None,
            analysis_limit: 0,
            writable_schema: false,
            defensive: false,
            imposters: Vec::new(),
            authorizer: None,
            vfs: std::sync::Arc::clone(&vfs),
            query_only: false,
            recursive_triggers: false,
            max_page_count: crate::pragma::DEFAULT_MAX_PAGE_COUNT,
            temp_store: 0,
            collations: Vec::new(),
            registry: modules(),
            eponymous: Vec::new(),
            virtual_tables: HashMap::new(),
            vector_indexes: HashMap::new(),
            index_stages: std::cell::Cell::new((0, 0, 0, 0, 0, 0, 0)),
            catalog_generation: 0,
        };
        opened.settle_journal()?;
        opened.rebuild_tables()?;
        opened.refresh_catalog();
        // The modules are connected after the tables are loaded, because a
        // module's shadow tables have to exist before it can be connected to
        // them. Nothing did this before, so a reopened database holding a
        // search table answered "no such table" for it.
        opened.reconnect_modules()?;
        opened.rebuild_tables()?;
        opened.refresh_catalog();
        Ok(opened)
    }

    /// Returns the catalog a statement is bound against.
    ///
    /// Exposed so an instrument can time binding on its own. `plan` is parse,
    /// bind and logical planning together, and knowing that the three of them
    /// are 64% of compiling `SELECT 1` does not say which of the three to
    /// change.
    pub fn catalog_view(&self) -> &StaticCatalog {
        &self.catalog
    }

    /// Returns every object's name and the identifier its tree is known by.
    ///
    /// The identifier is what the log refers to a tree by, so it is the thing
    /// two processes have to agree about. This exposes it so that agreement can
    /// be *tested* rather than assumed - a writer and a reader that disagreed
    /// would corrupt a recovery quietly, and the only cheap way to catch a
    /// caller reintroducing a process-local number is to compare the two.
    pub fn tree_identifiers(&self) -> Vec<(String, u64)> {
        self.entries
            .iter()
            .map(|held| {
                (
                    String::from_utf8_lossy(&held.entry.name).into_owned(),
                    held.entry.tree_id,
                )
            })
            .collect()
    }

    /// Returns the tables the import could not take.
    ///
    /// A caller that finds a query refused can tell "the engine does not do
    /// this yet" from "the table is not there" by looking here.
    pub fn skipped(&self) -> &[String] {
        &self.skipped
    }

    /// Returns how many frames the pool holds.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Returns how many bytes the pool occupies.
    pub fn pool_bytes(&self) -> usize {
        self.database.pool().byte_size()
    }

    /// Returns the database file the import wrote.
    pub fn file(&self) -> &std::path::Path {
        &self.path
    }

    /// Returns how many pages the file holds.
    pub fn page_count(&self) -> u64 {
        self.database.pool().page_count()
    }

    /// Returns how many pool frames hold a page right now.
    ///
    /// The measurable half of "what does the engine have in memory": the pool
    /// is where a database's pages live, and a frame count times the page size
    /// is the part of the resident set the engine chose rather than the part
    /// the allocator happens to be holding.
    pub fn frames_resident(&self) -> usize {
        self.database.pool().resident()
    }

    /// Returns what the pool has done since the last reset.
    pub fn pool_stats(&self) -> inillucent_pool::PoolStats {
        self.database.pool().stats()
    }

    /// Forgets the pool's counters, so a measurement starts from zero.
    pub fn reset_pool_stats(&self) {
        self.database.pool().reset_stats();
    }

    /// Reads every page of every tree, so a measurement starts warm.
    ///
    /// A cold pool measures the file system, and neither engine's scorecard
    /// number is about that. SQLite's arm is warmed by the harness running the
    /// workload before it times it; this is the same courtesy on this side, and
    /// it is stated rather than left to the first round.
    pub fn warm(&self) -> DbResult<()> {
        for tree in self.trees.values() {
            tree.visit_leaves(self.database.pool(), &mut |_| Ok(true))?;
        }
        Ok(())
    }

    /// Returns the bytes one root's tree occupies.
    ///
    /// Reported beside a measurement so a reader can see how much data each
    /// engine's chosen structure actually reads.
    ///
    /// @param root - the root page id the fixture recorded
    pub fn byte_size(&self, root: u32) -> Option<usize> {
        self.trees.get(&root).map(PagedTree::byte_size)
    }

    /// Returns the index roots that could cover a query over one table.
    ///
    /// @param table_root - the table's root page id
    pub fn candidates(&self, table_root: u32) -> Vec<u32> {
        self.covering.get(&table_root).cloned().unwrap_or_default()
    }

    /// Returns the page size the trees were built at.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Returns how many leaves one root's tree holds.
    ///
    /// @param root - the root page id the fixture recorded
    pub fn leaf_count(&self, root: u32) -> Option<usize> {
        self.trees.get(&root).map(|tree| tree.leaf_count() as usize)
    }

    /// Returns a table's root page id by name.
    ///
    /// The root page is the identifier everything else here is keyed by, and a
    /// measurement that wants "the main table" has only the name.
    ///
    /// @param name - the table's name
    pub fn table_root(&self, name: &str) -> Option<u32> {
        self.layouts
            .iter()
            .filter(|(root, _)| self.covering.contains_key(root))
            .map(|(root, _)| *root)
            .find(|root| self.catalog_name(*root).as_deref() == Some(name))
    }

    /// Returns the table name a root page belongs to.
    ///
    /// @param root - the root page id
    fn catalog_name(&self, root: u32) -> Option<String> {
        self.catalog
            .tables
            .iter()
            .find(|table| table.root == root)
            .map(|table| String::from_utf8_lossy(&table.name).into_owned())
    }

    /// Returns every imported root, for reporting.
    pub fn roots(&self) -> Vec<u32> {
        let mut roots: Vec<u32> = self.trees.keys().copied().collect();
        roots.sort_unstable();
        roots
    }

    /// Parses, binds and plans one statement.
    ///
    /// Separated from [`ImportedDatabase::run`] so the gate harness can plan
    /// once and execute many times, which is what `prepare_each: false` means
    /// in a scorecard plan.
    ///
    /// @param sql - the statement text
    ///
    /// **A refusal says what SQLite's says.** These used to wrap the parse or
    /// bind failure with `{error:?}`, so `SELECT * FROM nope` reported
    /// `SELECT * FROM nope;: ParseError { kind: Refused("no such table: nope"),
    /// span: Span { start: 0, end: 0 } }` where SQLite reports `no such table:
    /// nope`. The one-line message was there all along - `ParseError::message`
    /// - and printing the struct around it made every refusal look like a bug
    /// report about the engine rather than a sentence about the statement.
    pub fn plan(&self, sql: &str) -> DbResult<PhysicalPlan> {
        let parsed = self.parse_once(sql)?;
        let bound = self.bind_parsed(sql, &parsed);
        self.recycle(parsed);
        match bound? {
            BoundStatement::Select(select) => Ok(plan_select_with(*select, self.levers)),
            _ => Err(refusal(format!("{sql} is not a read-only statement"))),
        }
    }

    /// Runs a planned statement and returns its rows and column names.
    ///
    /// @param plan - a plan from [`ImportedDatabase::plan`]
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute(
        &self,
        plan: &PhysicalPlan,
        params: &Params,
    ) -> DbResult<(Vec<Vec<OwnedDatum>>, Vec<String>)> {
        // A compound is several plans and one answer, and a windowed query is a
        // plan with a pass on top of it; neither is a single prepared
        // statement, so both are dispatched by `run_any` rather than inside
        // `prepare` - which returns the structural choice for *one* pipeline.
        let (rows, shape) = physical::run_any(plan, self, params)?;
        Ok((rows, names_of(&shape)))
    }

    /// Chooses a statement's physical plan, once.
    ///
    /// Separated from execution because the choice depends on the statement and
    /// the schema and not on the data, and because making it per execution made
    /// a query answering 64 rows spend more time choosing a tree than reading
    /// one. `prepare once` in a scorecard plan means the same thing on both
    /// sides.
    ///
    /// @param plan - a plan from [`ImportedDatabase::plan`]
    pub fn prepare(&self, plan: &PhysicalPlan) -> DbResult<physical::Prepared> {
        physical::prepare(plan, self, ForcePlan::default())
    }

    /// Chooses a statement's physical plan under forced levers.
    ///
    /// The metamorphic tests' entry point: the same query under each
    /// alternative must produce the same digest.
    ///
    /// @param plan - a plan from [`ImportedDatabase::plan`]
    /// @param forced - the levers to apply
    pub fn prepare_forced(
        &self,
        plan: &PhysicalPlan,
        forced: ForcePlan,
    ) -> DbResult<physical::Prepared> {
        physical::prepare(plan, self, forced)
    }

    /// Runs an already-prepared statement.
    ///
    /// @param plan - a plan from [`ImportedDatabase::plan`]
    /// @param prepared - the choices [`ImportedDatabase::prepare`] made
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute_prepared(
        &self,
        plan: &PhysicalPlan,
        prepared: &physical::Prepared,
        params: &Params,
    ) -> DbResult<(Vec<Vec<OwnedDatum>>, Vec<String>)> {
        let (rows, shape) = physical::run_prepared(plan, self, prepared, params)?;
        Ok((rows, names_of(&shape)))
    }

    /// Builds a pipeline over an already-prepared statement.
    ///
    /// The measurement path: a caller hands in the sink it wants and drives the
    /// pipeline itself, so the timed region is the pipeline rather than a
    /// `Vec<Vec<OwnedDatum>>` neither engine's caller asked for.
    ///
    /// @param plan - a plan from [`ImportedDatabase::plan`]
    /// @param prepared - the choices [`ImportedDatabase::prepare`] made
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param sink - the end of the pipeline
    pub fn pipeline(
        &self,
        plan: &PhysicalPlan,
        prepared: &physical::Prepared,
        params: &Params,
        sink: Box<dyn inillucent_exec::Sink>,
    ) -> DbResult<(physical::Pipeline<'_>, physical::Shape)> {
        physical::build_prepared(plan, self, prepared, params, sink)
    }

    /// Builds a statement whose operator chain is reused across executions.
    ///
    /// The difference from [`ImportedDatabase::pipeline`] is the difference
    /// between preparing a *plan* and preparing a *statement*. A workload that
    /// binds new parameters and runs again is answered by SQLite from a VDBE
    /// program compiled once; `pipeline` rebuilt the operator chain each time,
    /// which `inillucent-probeprofile` measured at 42% of `point.rowid` and 71%
    /// of `point.miss`. This builds the chain once and rebuilds only the source
    /// whose key the parameters decide.
    ///
    /// @param plan - the planner's output
    /// @param prepared - the structural choices `prepare` made
    /// @param params - the values the first execution binds
    /// @param sink - the end of the pipeline, which the statement keeps
    pub fn statement<'a>(
        &'a self,
        plan: &'a PhysicalPlan,
        prepared: &physical::Prepared,
        params: &Params,
        sink: Box<dyn inillucent_exec::Sink>,
    ) -> DbResult<physical::Statement<'a>> {
        physical::build_statement(plan, self, prepared, params, sink)
    }

    /// Parses, plans and runs one statement.
    ///
    /// @param sql - the statement text
    pub fn run(&self, sql: &str) -> DbResult<(Vec<Vec<OwnedDatum>>, Vec<String>)> {
        let plan = self.plan(sql)?;
        self.execute(&plan, &Params::new())
    }

    /// Parses, plans and runs one statement with parameters bound.
    ///
    /// @param sql - the statement text
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn run_with(
        &self,
        sql: &str,
        params: &Params,
    ) -> DbResult<(Vec<Vec<OwnedDatum>>, Vec<String>)> {
        let plan = self.plan(sql)?;
        self.execute(&plan, params)
    }

    /// Returns the physical operator list a prepared statement will run.
    ///
    /// Printed beside SQLite's `EXPLAIN QUERY PLAN` so a reader can see which
    /// structure each engine chose. A ratio measured against a different
    /// structure is not a ratio between engines, which is the single largest
    /// thing Phase 1 learned.
    ///
    /// @param prepared - the choices [`ImportedDatabase::prepare`] made
    pub fn describe_physical(&self, prepared: &physical::Prepared) -> Vec<String> {
        prepared.describe()
    }

    /// Returns the `EXPLAIN QUERY PLAN` lines a statement's plan renders as.
    ///
    /// The harness prints these beside SQLite's so a reader can see whether the
    /// two engines chose the same structure. A ratio measured against a
    /// different structure is not a ratio between engines.
    ///
    /// @param sql - the statement text
    pub fn describe(&self, sql: &str) -> DbResult<Vec<String>> {
        Ok(self.plan(sql)?.describe())
    }

    /// Returns the step listing a plain `EXPLAIN` of a statement would print.
    ///
    /// Compiled through the same cache the statement itself uses, because the
    /// listing is *of* the compiled statement: a listing built from a fresh
    /// plan could describe a plan the next execution would not get.
    ///
    /// @param sql - the statement to list
    pub(crate) fn program_listing(
        &self,
        sql: &str,
    ) -> DbResult<Vec<(String, i64, i64, String, String)>> {
        match &*self.compiled(&format!("EXPLAIN {sql}"))? {
            Cached::Program(listing) => Ok(listing.clone()),
            _ => Ok(Vec::new()),
        }
    }

    /// Returns the objects a statement names, and which one it writes.
    ///
    /// The plan's sources rather than the parse's, so a name that resolved to a
    /// view is reported as the view and the tables behind it are reported too.
    ///
    /// @param sql - the statement to describe
    pub(crate) fn statement_tables(&self, sql: &str) -> DbResult<StatementTables> {
        let parsed = self.parse_once(sql)?;
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
            .with_foreign_keys(self.foreign_keys, self.defer_foreign_keys);
        let bound = binder.bind_statement(&parsed.statement).map_err(refused)?;
        let mut names: Vec<(&'static str, Vec<u8>)> = Vec::new();
        let mut written = None;
        match &bound {
            BoundStatement::Select(select) => collect_sources(select, &mut names),
            BoundStatement::Insert(statement) => written = Some(statement.table.name.clone()),
            BoundStatement::Update(statement) => written = Some(statement.table.name.clone()),
            BoundStatement::Delete(statement) => written = Some(statement.table.name.clone()),
            _ => {}
        }
        if let Some(target) = &written {
            names.insert(0, ("table", target.clone()));
        }
        names.dedup();
        Ok((names, written))
    }

    /// Returns how many columns a cached statement answers with, and whether
    /// it only reads.
    ///
    /// @param sql - the statement text
    pub(crate) fn statement_shape(&self, sql: &str) -> (i64, bool) {
        let columns = match self.compiled(sql) {
            Ok(cached) => match &*cached {
                Cached::Select(plan, _) => plan.select.columns.len() as i64,
                _ => 0,
            },
            Err(_) => 0,
        };
        let reads = self
            .statement_tables(sql)
            .map(|(_, written)| written.is_none())
            .unwrap_or(true);
        (columns, reads)
    }

    /// Returns the physical operators a statement runs **through the cache**.
    ///
    /// The difference from [`ImportedDatabase::describe`] is the whole point of
    /// it: that one plans afresh every time, so it could not tell a live cache
    /// from an invalidated one. This asks for the compiled statement the next
    /// execution would get, which is the object a DDL statement has to throw
    /// away.
    ///
    /// @param sql - the statement text
    pub fn describe_cached(&self, sql: &str) -> DbResult<Vec<String>> {
        match &*self.compiled(sql)? {
            Cached::Nothing => Ok(vec!["nothing".to_string()]),
            Cached::Select(_, prepared) => Ok(prepared.describe()),
            Cached::Ddl(_) => Ok(vec!["a directive".to_string()]),
            Cached::QueryPlan(_) => Ok(vec!["a query plan".to_string()]),
            Cached::Program(_) => Ok(vec!["a program listing".to_string()]),
            Cached::Insert(..) => Ok(vec!["an insert".to_string()]),
            Cached::VirtualInsert(_) => Ok(vec!["an insert into a module".to_string()]),
            Cached::VirtualUpdate(..) => Ok(vec!["an update of a module".to_string()]),
            Cached::VirtualDelete(..) => Ok(vec!["a delete from a module".to_string()]),
            Cached::Update(..) => Ok(vec!["an update".to_string()]),
            Cached::Delete(..) => Ok(vec!["a delete".to_string()]),
        }
    }

    /// Applies a statement's row images to every index a module owns.
    ///
    /// **The other half of `Changes::written`.** The write path cannot reach a
    /// module, so it reports what it stored and what it removed; this is where
    /// those become the module's own inserts and deletes. Removals go first,
    /// because an `UPDATE` reports both halves of the same key and the store
    /// would otherwise hold the old row and refuse the new one.
    ///
    /// A row whose vector is NULL is not in the index at all, which is what
    /// makes a partly-populated column work: the rows that have vectors are
    /// searchable and the rows that do not are simply absent.
    ///
    /// @param changes - what the statement stored and removed
    pub(crate) fn follow_vector_indexes(&mut self, changes: &Changes) -> DbResult<()> {
        if changes.written.is_empty() && changes.removed.is_empty() {
            return Ok(());
        }
        let indexes: Vec<VectorIndex> = self.vector_indexes.values().flatten().cloned().collect();
        for index in indexes {
            for row in &changes.removed {
                let Some(OwnedDatum::Int(rowid)) = row.get(index.rowid) else {
                    continue;
                };
                self.change_module(
                    &index.name,
                    &inillucent_sql::vtab::Change::Delete(inillucent_value::Value::Integer(*rowid)),
                )?;
            }
            for row in &changes.written {
                let Some(OwnedDatum::Int(rowid)) = row.get(index.rowid) else {
                    continue;
                };
                let Some(vector) = row.get(index.column) else {
                    continue;
                };
                let value = inillucent_exec::scalar::to_value(vector.borrow());
                if matches!(value, inillucent_value::Value::Null) {
                    continue;
                }
                self.change_module(
                    &index.name,
                    &inillucent_sql::vtab::Change::Insert {
                        rowid: inillucent_value::Value::Integer(*rowid),
                        // `body` then the hidden query columns: the store's
                        // first declared column carries the source rowid as
                        // text, so a hit can name the row it came from, and the
                        // vector goes in the hidden `vector` column the module
                        // reads embeddings out of.
                        values: vec![
                            inillucent_value::Value::owned_text(rowid.to_string().as_bytes())?,
                            inillucent_value::Value::Null,
                            inillucent_value::Value::Null,
                            value,
                            inillucent_value::Value::Null,
                            inillucent_value::Value::Null,
                        ],
                    },
                )?;
            }
        }
        Ok(())
    }

    /// Asks an index a module owns for the rowids nearest a vector.
    ///
    /// **The store's rowid is the table's rowid**, which is what makes this an
    /// answer rather than a lookup table: the index was written with the source
    /// row's number as its own, so the candidates come back ready to probe the
    /// table with.
    ///
    /// The query is the module's own vector-only shape - no query text, a
    /// vector, and a depth - and it is put through the ordinary planner, so
    /// there is one implementation of what asking this module means.
    ///
    /// @param index - the store's name
    /// @param probe - the vector to measure against
    /// @param depth - how many candidates to ask for
    fn nearest_rowids(
        &self,
        index: &[u8],
        probe: &inillucent_tree::datum::Datum<'_>,
        depth: usize,
    ) -> DbResult<Option<Vec<i64>>> {
        let folded = index.to_ascii_lowercase();
        let Some(connected) = self.virtual_tables.get(&folded) else {
            return Ok(None);
        };
        let (inillucent_tree::datum::Datum::Blob(bytes)
        | inillucent_tree::datum::Datum::Text(bytes)) = probe
        else {
            // A probe that is not bytes cannot be a vector, and an index asked
            // for the nearest to a number has no answer rather than a wrong
            // one.
            return Ok(Some(Vec::new()));
        };
        Ok(Some(self.probe_module(connected, bytes, depth)?))
    }

    /// Puts the module's own vector-only query to one connected store.
    ///
    /// **Built here rather than compiled from text**, because this runs inside
    /// a read: the executor is holding the catalog, and compiling a statement
    /// would want the connection mutably. The constraints are exactly the three
    /// the module documents - no query text, a vector, and a depth - offered
    /// through `best_index` the way the planner offers them, so the module
    /// chooses its own plan rather than being told one.
    ///
    /// @param connected - the store
    /// @param probe - the vector's bytes
    /// @param depth - how many candidates to ask for
    fn probe_module(
        &self,
        connected: &vtab::Connected,
        probe: &[u8],
        depth: usize,
    ) -> DbResult<Vec<i64>> {
        use inillucent_sql::vtab::{ConstraintOp, ConstraintSpec, IndexQuery, OrderSpec};
        let declaration = connected.table.declaration();
        let column_of = |wanted: &[u8]| -> Option<i32> {
            declaration
                .columns
                .iter()
                .position(|held| held.name.eq_ignore_ascii_case(wanted))
                .and_then(|at| i32::try_from(at).ok())
        };
        // The query column is the table's own name; the rest are named.
        let (Some(query), Some(k), Some(vector), Some(rank)) = (
            column_of(&connected.arguments.table),
            column_of(b"k"),
            column_of(b"vector"),
            column_of(b"rank"),
        ) else {
            return Err(refusal("the index's store is not a search table"));
        };
        let specs = vec![
            ConstraintSpec {
                column: query,
                op: ConstraintOp::Match,
                usable: true,
            },
            ConstraintSpec {
                column: vector,
                op: ConstraintOp::Eq,
                usable: true,
            },
            ConstraintSpec {
                column: k,
                op: ConstraintOp::Eq,
                usable: true,
            },
        ];
        let values = [
            inillucent_value::Value::owned_text(b"")?,
            inillucent_value::Value::owned_blob(probe)?,
            inillucent_value::Value::Integer(depth as i64),
        ];
        let mut query_plan = IndexQuery::new(
            specs,
            vec![OrderSpec {
                column: rank,
                descending: false,
            }],
        );
        connected.table.best_index(&mut query_plan)?;
        let mut arguments: Vec<inillucent_value::Value<'static>> = Vec::new();
        for position in query_plan.argument_order() {
            let Some(value) = values.get(position) else {
                continue;
            };
            arguments.push(value.clone());
        }
        let plan = inillucent_ext::vtab::FilterPlan {
            index_number: query_plan.index_number,
            index_string: query_plan.index_string.clone(),
            arguments,
        };
        let mut cursor = connected.table.open()?;
        let store = vtab::ReadStore {
            pool: self.database.pool(),
            trees: &self.trees,
        };
        let mut nowhere = inillucent_ext::vtab::WithStore { store };
        let mut context = inillucent_ext::vtab::Context {
            host: &mut nowhere,
            database: 0,
            limits: &self.limits,
            catalog: None,
        };
        cursor.filter(&mut context, &plan)?;
        let mut found = Vec::with_capacity(depth);
        while !cursor.eof() {
            found.push(cursor.rowid()?);
            cursor.next(&mut context)?;
        }
        Ok(found)
    }

    /// Rebuilds the map of indexes a module owns from the connected tables.
    ///
    /// Called wherever the catalog changes. The association is read back out of
    /// the arguments the engine itself wrote when the index was created, which
    /// is why this does not have to parse a module's argument grammar in
    /// general: it only recognises the two arguments it put there.
    pub(crate) fn refresh_vector_indexes(&mut self) {
        let mut found: HashMap<u32, Vec<VectorIndex>> = HashMap::new();
        for (name, connected) in &self.virtual_tables {
            let Some(source) = argument_of(&connected.arguments.arguments, b"source") else {
                continue;
            };
            let Some(column) = argument_of(&connected.arguments.arguments, b"source_column") else {
                continue;
            };
            let folded = source.to_ascii_lowercase();
            let Some(table) = self.tables.iter().find(|held| held.folded == folded) else {
                continue;
            };
            let Some(layout) = self.layouts.get(&table.root) else {
                continue;
            };
            let wanted = column.to_ascii_lowercase();
            let Some(position) = table.columns.iter().position(|held| held.folded == wanted) else {
                continue;
            };
            let (Some(slot), Some(rowid)) =
                (layout.slots.get(position).copied().flatten(), layout.rowid)
            else {
                continue;
            };
            found.entry(table.root).or_default().push(VectorIndex {
                name: name.clone(),
                column: slot,
                rowid,
                declared: position as u16,
            });
        }
        // **Published into the catalog as well, because the planner reads the
        // catalog and not this map.** An index a module owns is an `IndexInfo`
        // with `IndexOrigin::Module` on the table it indexes: none of the
        // b-tree paths apply to it, and the one path that does looks for
        // exactly that origin.
        for table in &mut self.tables {
            table
                .indexes
                .retain(|held| held.origin != inillucent_sql::catalog_view::IndexOrigin::Module);
            let Some(indexes) = found.get(&table.root) else {
                continue;
            };
            for index in indexes {
                table.indexes.push(inillucent_sql::catalog_view::IndexInfo {
                    folded: index.name.to_ascii_lowercase(),
                    name: index.name.clone(),
                    root: 0,
                    unique: false,
                    columns: vec![inillucent_sql::catalog_view::IndexColumnInfo {
                        column: Some(index.declared),
                        collation: b"binary".to_vec(),
                        descending: false,
                        declared_descending: false,
                        expr_sql: None,
                    }],
                    partial_sql: None,
                    origin: inillucent_sql::catalog_view::IndexOrigin::Module,
                    conflict: None,
                    prefix_rows: Vec::new(),
                    analysed_rows: None,
                });
            }
        }
        self.vector_indexes = found;
    }

    /// Returns where the last `CREATE INDEX` spent its time.
    ///
    /// Milliseconds per stage, rendered for a report.
    pub fn build_stages(&self) -> String {
        let (scan, sort, unique, flatten, pack, catalog, seal) = self.index_stages.get();
        format!(
            "scan {:.1} ms, sort {:.1} ms, unique {:.1} ms, flatten {:.1} ms, pack {:.1} ms, catalog {:.1} ms, seal {:.1} ms",
            scan as f64 / 1e6,
            sort as f64 / 1e6,
            unique as f64 / 1e6,
            flatten as f64 / 1e6,
            pack as f64 / 1e6,
            catalog as f64 / 1e6,
            seal as f64 / 1e6
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
    /// Scan, sort, uniqueness check, flatten, pack, catalog, seal.
    ///
    /// `flatten` is the arena being read out into the run of `Datum`s the bulk
    /// builder walks. It is separate from `pack` because the two are different
    /// claims - one is a copy this ticket could still remove, the other is the
    /// tree being written - and folding them together is how the copy hid.
    pub fn build_stage_nanos(&self) -> (u128, u128, u128, u128, u128, u128, u128) {
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

    /// Returns whether every statement is its own transaction.
    ///
    /// `false` between a `BEGIN` and its `COMMIT`, which is what
    /// `sqlite3_get_autocommit` answers and what the differential harness
    /// compares after every step.
    pub fn autocommit(&self) -> bool {
        self.batch.get().is_none()
    }

    /// Returns the rowid the last `INSERT` assigned on this database.
    pub fn last_insert_rowid(&self) -> i64 {
        self.last_rowid.get()
    }

    /// Returns how many rows every statement so far has changed.
    pub fn total_changes(&self) -> i64 {
        self.changed_ever.get()
    }

    /// Returns how many rows the most recent write changed.
    ///
    /// What `sqlite3_changes` and the `changes()` scalar answer. The
    /// statement's own rows: a trigger body's go into `total_changes` and not
    /// into this, which is SQLite's rule.
    pub fn changes(&self) -> i64 {
        self.last_changes.get()
    }

    /// Records what a write changed, on both counters.
    ///
    /// @param own - the rows the statement wrote itself
    /// @param all - the rows written under it, triggers included
    fn record_changes(&self, own: i64, all: i64) {
        self.last_changes.set(own);
        self.changed_ever
            .set(self.changed_ever.get().saturating_add(all));
    }

    /// Records the rowid an `INSERT` assigned, when it assigned one.
    ///
    /// @param rowid - the key, or `None` when the statement wrote no row into a
    ///   table that has one
    fn remember_rowid(&self, rowid: Option<i64>) {
        if let Some(assigned) = rowid {
            self.last_rowid.set(assigned);
        }
    }

    /// Returns what a statement's scalars should be told about the connection.
    ///
    /// `changes()`, `total_changes()` and `last_insert_rowid()` are constants
    /// for the length of one statement - SQLite updates them when a statement
    /// *finishes* - so they are read once here and compiled in, rather than
    /// asked per row.
    pub fn scalar_context(&self) -> inillucent_exec::scalar::Context {
        inillucent_exec::scalar::Context {
            changes: self.last_changes.get(),
            total_changes: self.changed_ever.get(),
            last_insert_rowid: self.last_rowid.get(),
            seed: self.next_seed(),
            // So the `like(a, b)` function spelling follows the same pragma the
            // `LIKE` operator does.
            like_case_sensitive: self.case_sensitive_like,
        }
    }

    /// Returns a fresh seed for the random built-ins.
    ///
    /// **`random()` used to answer the same number for ever**, in
    /// every statement of every connection, because the new engine called the
    /// function library with a default context and the default seed is zero.
    /// One number is a legal answer to one call and a wrong answer to two, and
    /// an application seeding anything from it - a token, a sample, a shuffle -
    /// got a constant with no way to notice.
    ///
    /// The stream is `xoshiro256**`, started from the clock and the process id
    /// so two connections opened in the same millisecond do not share it, and
    /// advanced once per statement. Not cryptographic, which is also true of
    /// SQLite's `random()`.
    fn next_seed(&self) -> u64 {
        let mut rng = inillucent_base::rng::Rng::new(self.seed.get());
        let next = rng.next_u64();
        self.seed.set(next);
        next
    }

    /// Opens a transaction that the statements after it all join.
    ///
    /// The difference between this and autocommit is the whole of what a commit
    /// costs, which is the gate's `transaction` family. Calling it twice without
    /// a commit between keeps the first transaction, because that is what
    /// `BEGIN` inside a transaction does.
    pub fn begin_batch(&mut self) {
        if self.batch.get().is_some() {
            return;
        }
        // **The file is taken here, not at the first write.** A transaction
        // that raised its lock halfway through could be refused halfway
        // through, with statements already applied; taking it at `BEGIN` means
        // a transaction that starts is a transaction that can finish. It is
        // also why this engine's transactions serialise across processes rather
        // than overlapping: there is no shared-memory index that would let a
        // reader follow a writer's log, and pretending otherwise is what would
        // corrupt a file.
        let _ = self.database.begin_write_within(true);
        let txn = self.next_txn.get();
        self.next_txn.set(txn.saturating_add(1));
        self.batch.set(Some(txn));
        self.undo.borrow_mut().clear();
        self.marks.clear();
        self.touched = 0;
    }

    /// Undoes everything the open transaction changed, newest first.
    ///
    /// **Newest first, and that is the whole of the ordering rule.** A key
    /// written twice inside one transaction has two records; restoring the
    /// older one last is what puts the row back the way it was before the
    /// transaction rather than the way it was in the middle of it.
    ///
    /// The restores are ordinary writes and are logged like any other, because
    /// the log is redo-only: a crash between the rollback and the commit record
    /// has to replay to the *rolled back* state, not to the state the aborted
    /// statements left. Undoing by not-logging would leave the log describing
    /// changes the file no longer has.
    ///
    /// @param to - the savepoint to stop at, or `None` for the whole transaction
    fn undo_to(&mut self, to: Option<&[u8]>) -> DbResult<()> {
        let floor = match to {
            Some(name) => {
                let folded = name.to_ascii_lowercase();
                let Some(position) = self
                    .marks
                    .iter()
                    .rposition(|(held, _)| *held == folded)
                    .map(|index| self.marks.get(index).map(|(_, at)| *at).unwrap_or(0))
                else {
                    return Err(refusal(format!(
                        "no such savepoint: {}",
                        String::from_utf8_lossy(name)
                    )));
                };
                position
            }
            None => 0,
        };
        self.undo_to_floor(floor, true, self.current_txn())
    }

    /// Undoes back to a position in the undo buffer, newest first.
    ///
    /// The body of [`ImportedDatabase::undo_to`], separated so a *statement*
    /// can name its own floor. A savepoint is a name this connection was given;
    /// a statement boundary is a length nobody named, taken by `write` before
    /// the statement wrote anything, and there is nothing to look up.
    ///
    /// @param floor - the buffer length to stop at
    /// @param reload - whether to rebuild the schema from the catalog tree
    /// @param txn - the transaction the restores are logged under, which is the
    ///   statement's own rather than `current_txn`: outside a batch `write` has
    ///   already taken a number and moved `next_txn` past it
    fn undo_to_floor(&mut self, floor: usize, reload: bool, txn: u64) -> DbResult<()> {
        while self.undo.borrow().len() > floor {
            let Some(entry) = self.undo.borrow_mut().pop() else {
                break;
            };
            // **The record says which file it came out of, and that is the
            // whole of the routing.** Two databases each number their own trees
            // from one, so the identifier alone is ambiguous the moment a
            // connection holds a second file - and a rollback that guessed
            // would put a row back into the wrong database, silently.
            let at = entry.schema;
            let wal = self
                .log_of(at)
                .ok_or_else(|| refusal("a rollback names a database that is not attached"))?;
            let mut log = WalLog {
                wal,
                txn,
                schema: at,
                wrote: false,
                // The restore is not itself undoable: it *is* the undo, and
                // recording it would grow the buffer being drained.
                undo: None,
            };
            // **The catalog tree answers to two numbers.** Its `tree_id` is
            // `SCHEMA_TREE_ID`, which is what the log records carry, and it
            // lives in `trees` under the handle the planner reads
            // `sqlite_schema` through. An undo record carries the first and
            // this map is keyed by the second, so a catalog row's before-image
            // was looked up under a number nothing held and silently skipped -
            // which is why a rolled-back `CREATE TABLE` stayed in the schema.
            let root = if entry.tree == inillucent_catalog::paged::SCHEMA_TREE_ID {
                self.catalog_handle_of(at)
            } else {
                self.handle_of(at, entry.tree).unwrap_or(0)
            };
            let Some(tree) = self.trees.get_mut(&root) else {
                // The tree is gone, which a rollback of a `CREATE TABLE` makes
                // true. Its rows went with it.
                continue;
            };
            let session = self.session.get();
            let database = file_of(
                &mut self.database,
                &mut self.attached,
                &mut self.temps,
                session,
                at,
            )?;
            match &entry.row {
                Some(row) => {
                    let values: Vec<Datum<'_>> = row.iter().map(OwnedDatum::borrow).collect();
                    tree.put(database, &mut log, &values)?;
                }
                None => {
                    let key: Vec<Datum<'_>> = entry.key.iter().map(OwnedDatum::borrow).collect();
                    tree.delete(database, &mut log, &key)?;
                }
            }
        }
        let held = self.undo.borrow().len();
        self.marks.retain(|(_, at)| *at <= held);
        // **A DML statement cannot have changed the catalog, so undoing one has
        // nothing to rebuild from it.** `CREATE`, `DROP` and `ALTER` do not go
        // through `write`, and reloading here would cost a catalog read on
        // every failed statement - and could replace a constraint failure with
        // the "no tree attached" refusal below, which would be a different
        // error for a statement that never touched a table's existence.
        if !reload {
            return Ok(());
        }
        // The catalog tree may have been restored along with everything else,
        // so the schema the binder sees is rebuilt from it.
        let missing = self.reload_entries()?;
        if !missing.is_empty() {
            // A `DROP` undone by restoring its catalog row puts the object back
            // in the schema without putting its tree back in this handle, and a
            // table the binder names and nothing can read is a wrong answer
            // waiting to happen. Refused by name until the re-attach is written.
            return Err(refusal(format!(
                "rolling back left {} in the schema with no tree attached;                  undoing a DROP inside a transaction is not supported yet",
                missing.join(", ")
            )));
        }
        Ok(())
    }

    /// Rebuilds the in-memory schema from the catalog tree.
    ///
    /// **The catalog tree is the authority and `entries` is a cache of it.** A
    /// rollback restores the tree - a catalog row is a row, and it is undone
    /// like one - and this is what makes the cache agree again. Without it a
    /// `CREATE TABLE` that was abandoned stayed visible: the row was gone from
    /// the file and still in the list the binder is built from.
    ///
    /// It does not re-attach trees. Every object it names that has no tree is
    /// reported, because a schema naming a table nothing can read is worse than
    /// a refusal - see [`ImportedDatabase::undo_to`], which turns that into
    /// one.
    fn reload_entries(&mut self) -> DbResult<Vec<String>> {
        let mut missing = Vec::new();
        // Every schema, because a transaction may have written more than one of
        // them and a rollback restores every file it touched.
        for at in self.schema_numbers() {
            let Some(database) = self.schema_file(at) else {
                continue;
            };
            let catalog_tree = attach_catalog(database.pool(), database.catalog_root())?;
            let stored =
                inillucent_catalog::paged::read_catalog_rows(database.pool(), &catalog_tree)?;
            let reloaded: Vec<Recorded> = stored
                .into_iter()
                .map(|(rowid, entry)| {
                    let root = self.handle_of(at, entry.tree_id).unwrap_or(0);
                    Recorded { rowid, root, entry }
                })
                .collect();
            // **A `DROP` undone by restoring its catalog row has to get its
            // tree handle back too.** `release_tree` takes the handle out of
            // this connection when the object is dropped, and a rollback
            // restores the *rows* - the catalog row and every page of the tree,
            // which the undo log holds - but not the handle, because a handle
            // is not a row. Before this was fixed, the object came back into the
            // schema with nothing behind it, the rollback was refused, and the
            // connection was then unable to read the table at all: `no layout
            // imported for root page 2147483648`. A refusal that damages the
            // session is worse than one that does not.
            //
            // The shape comes from the entry's own `CREATE` text, which is the
            // same place `define_table` derives it from - so a re-attached tree
            // is described exactly as the original was rather than from
            // whatever this process happens to remember.
            let restored = self.reattach_entries(at, &reloaded)?;
            let reloaded: Vec<Recorded> = reloaded
                .into_iter()
                .map(|mut held| {
                    if let Some(root) = restored.get(&held.rowid) {
                        held.root = *root;
                    }
                    held
                })
                .collect();
            for held in &reloaded {
                if held.entry.tree_id != 0 && !self.trees.contains_key(&held.root) {
                    missing.push(String::from_utf8_lossy(&held.entry.name).into_owned());
                }
            }
            if let Some(held) = self.entries_of_mut(at) {
                *held = reloaded;
            }
        }
        self.rebuild_tables()?;
        self.refresh_catalog();
        Ok(missing)
    }

    /// Re-attaches the tree of every restored entry this connection has lost.
    ///
    /// Called from [`ImportedDatabase::reload_entries`] after a rollback has put
    /// the catalog rows back. Returns the handle each restored entry ended up
    /// with, by catalog rowid, so the caller can correct its own list.
    ///
    /// An entry whose shape cannot be derived is left alone rather than
    /// reported here: the caller's `missing` check is what turns that into a
    /// refusal, and reporting it twice would report a rollback that half worked
    /// as two different failures.
    ///
    /// @param at - which attached database
    /// @param entries - the entries as the catalog tree now holds them
    fn reattach_entries(
        &mut self,
        at: usize,
        entries: &[Recorded],
    ) -> DbResult<std::collections::HashMap<i64, u32>> {
        let mut restored = std::collections::HashMap::new();
        for held in entries {
            if held.entry.tree_id == 0 {
                continue;
            }
            let root = match self.handle_of(at, held.entry.tree_id) {
                Some(root) if root != 0 => root,
                _ => continue,
            };
            if self.trees.contains_key(&root) {
                continue;
            }
            let Some((columns, key_columns, layout)) = self.shape_of_entry(entries, held, root)
            else {
                continue;
            };
            let Some(database) = self.schema_file(at) else {
                continue;
            };
            let tree = PagedTree::attach(
                database.pool(),
                held.entry.tree_id,
                held.entry.root,
                columns,
                key_columns,
                held.entry.stats.leaf_count,
                held.entry.stats.row_count,
            )?;
            self.trees.insert(root, tree);
            self.layouts.insert(root, std::rc::Rc::new(layout));
            self.owner.insert(root, at);
            restored.insert(held.rowid, root);
        }
        // An index's tree is a covering candidate of its table's, and the link
        // went with the handle when the object was dropped.
        for held in entries {
            if held.entry.kind != ObjectKind::Index {
                continue;
            }
            let (Some(index_root), Some(table_root)) = (
                restored.get(&held.rowid).copied(),
                self.root_of_named(entries, at, &held.entry.table),
            ) else {
                continue;
            };
            // A `DROP` that is rolled back puts the index's tree back, and
            // it may only rejoin the covering set on the same terms it was in
            // it: the catalog text is what says whether it is partial.
            let partial = inillucent_catalog::load::index_from_create_sql(
                &held.entry.sql,
                &TableInfo::subquery(held.entry.table.clone(), 0, Vec::new()),
                index_root,
            )
            .map(|index| index.partial_sql.is_some())
            .unwrap_or(true);
            if partial {
                continue;
            }
            let candidates = self.covering.entry(table_root).or_default();
            if !candidates.contains(&index_root) {
                candidates.push(index_root);
            }
        }
        Ok(restored)
    }

    /// Returns the handle of a table named by one of the restored entries.
    ///
    /// @param entries - the entries as the catalog tree now holds them
    /// @param at - which attached database
    /// @param name - the table's name
    fn root_of_named(&self, entries: &[Recorded], at: usize, name: &[u8]) -> Option<u32> {
        let folded = name.to_ascii_lowercase();
        entries
            .iter()
            .find(|held| {
                held.entry.kind == ObjectKind::Table
                    && held.entry.name.to_ascii_lowercase() == folded
            })
            .and_then(|held| self.handle_of(at, held.entry.tree_id))
    }

    /// Derives one entry's tree shape from its stored `CREATE` text.
    ///
    /// The same derivation `define_table` and `create_index` make, from the same
    /// source: the text in the catalog row. An automatic index carries no text
    /// of its own, so it is reconstructed from its table's - which is what the
    /// loader does for one too.
    ///
    /// @param entries - the entries as the catalog tree now holds them
    /// @param held - the entry whose tree is being rebuilt
    /// @param root - the handle it will be registered under
    fn shape_of_entry(
        &self,
        entries: &[Recorded],
        held: &Recorded,
        root: u32,
    ) -> Option<(Vec<ColumnSpec>, usize, SourceLayout)> {
        match held.entry.kind {
            ObjectKind::Table => {
                let info = table_from_create_sql(&held.entry.sql, 0, root).ok()?;
                if info.without_rowid {
                    keyed_table_shape(&info).ok()
                } else {
                    let (columns, layout) = table_shape(&info);
                    Some((columns, 1, layout))
                }
            }
            ObjectKind::Index => {
                let owner = entries.iter().find(|other| {
                    other.entry.kind == ObjectKind::Table
                        && other.entry.name.eq_ignore_ascii_case(&held.entry.table)
                })?;
                let table = table_from_create_sql(&owner.entry.sql, 0, owner.root).ok()?;
                // An automatic index is declared by the *table's* text, and a
                // created one by its own - the catalog stores an empty `sql`
                // for the first, which is what tells the two apart.
                let mut index = if held.entry.sql.is_empty() {
                    let folded = held.entry.name.to_ascii_lowercase();
                    table
                        .indexes
                        .iter()
                        .find(|index| index.folded == folded)?
                        .clone()
                } else {
                    inillucent_catalog::load::index_from_create_sql(&held.entry.sql, &table, root)
                        .ok()?
                };
                index.root = root;
                let (columns, layout) = index_shape(&table, &index, root);
                let key_columns = columns.len();
                Some((columns, key_columns, layout))
            }
            _ => None,
        }
    }

    /// Abandons the open transaction.
    ///
    /// **Every step runs, and the first failure is reported afterwards.** A
    /// rollback is not a step that can be declined: the caller has said the
    /// transaction is over, and returning early from the middle of it would
    /// leave the rows undone and the connection still believing a transaction
    /// and its savepoints were open - a state with no name, which the next
    /// statement would inherit. So the module notification, the undo and the
    /// bookkeeping all happen, and only then is an error returned.
    pub fn rollback(&mut self) -> DbResult<()> {
        // **The modules are told, or the connection goes on answering out of a
        // transaction that did not happen.** See `rollback_modules`: the file
        // was always put back correctly, and the module's own buffer was not.
        let told = self.rollback_modules(None);
        let undone = self.undo_to(None);
        self.marks.clear();
        self.batch.set(None);
        // Nothing to decide: an abandoned transaction has no commit for a
        // super-journal to be about, and the records it left are never replayed
        // because no `Commit` follows them.
        self.touched = 0;
        // The transaction's own setting goes with the transaction, which is
        // SQLite's rule for `PRAGMA defer_foreign_keys`.
        self.defer_foreign_keys = false;
        self.refresh_catalog();
        undone?;
        told?;
        Ok(())
    }

    /// Names a point the transaction can be rolled back to.
    ///
    /// **Every module is flushed here**, which is what makes rolling back to
    /// this point correct for a module that buffers. The undo log records
    /// writes to the shadow *trees*, so anything a module is still holding in
    /// memory is invisible to it - and a later `ROLLBACK TO` would either keep
    /// staged rows belonging to the abandoned part, or throw away rows written
    /// before the point. Flushing now puts everything before the point under
    /// the undo log, so the buffer that is left belongs entirely to the part
    /// that may be abandoned.
    ///
    /// A savepoint is rare and a flush is not free, which is the right way
    /// round: the alternative is a module buffer the undo log cannot see.
    ///
    /// @param name - the savepoint's name
    pub fn savepoint(&mut self, name: &[u8]) -> DbResult<()> {
        self.sync_modules()?;
        let held = self.undo.borrow().len();
        self.marks.push((name.to_ascii_lowercase(), held));
        Ok(())
    }

    /// Undoes back to a savepoint, keeping the transaction open.
    ///
    /// @param name - the savepoint's name
    pub fn rollback_to(&mut self, name: &[u8]) -> DbResult<()> {
        // **The level of the savepoint being returned to, not how deep the
        // nesting currently is.** `SAVEPOINT a; SAVEPOINT b; ROLLBACK TO a`
        // has two marks and a target level of zero, and a module told "two"
        // would keep the state belonging to `b` - the savepoint that was just
        // abandoned. A module numbers its own marks by what it was given, so
        // the number has to mean the same thing to both sides.
        //
        // A name the transaction does not hold is left to `undo_to` to refuse,
        // so that the error is the one it has always been.
        let folded = name.to_ascii_lowercase();
        let Some(position) = self.marks.iter().rposition(|(held, _)| *held == folded) else {
            // **A name no savepoint holds changes nothing, modules included.**
            // Defaulting the level to zero and telling the modules anyway made
            // `ROLLBACK TO a_name_that_is_not_open` discard a buffered virtual
            // table's pending writes and *then* report the error - a failed
            // statement with a side effect, which is the one thing a failed
            // statement may not have. `undo_to` refuses it below with the
            // message it has always used.
            self.undo_to(Some(name))?;
            self.refresh_catalog();
            return Ok(());
        };
        let level = i32::try_from(position).unwrap_or(i32::MAX);
        let told = self.rollback_modules(Some(level));
        let undone = self.undo_to(Some(name));
        self.refresh_catalog();
        undone?;
        told?;
        Ok(())
    }

    /// Forgets a savepoint without undoing anything.
    ///
    /// @param name - the savepoint's name
    pub fn release(&mut self, name: &[u8]) -> DbResult<()> {
        let folded = name.to_ascii_lowercase();
        let Some(position) = self.marks.iter().rposition(|(held, _)| *held == folded) else {
            return Err(refusal(format!(
                "no such savepoint: {}",
                String::from_utf8_lossy(name)
            )));
        };
        self.marks.truncate(position);
        Ok(())
    }

    /// Commits the open transaction, if there is one.
    ///
    /// A no-op outside a transaction, so a caller can commit at a boundary
    /// without having to know whether it opened one.
    pub fn commit_batch(&mut self) -> DbResult<()> {
        // **A deferred key is checked here, and a failure means the commit does
        // not happen.** That is SQLite's rule and the whole meaning of
        // `DEFERRABLE INITIALLY DEFERRED`: the rows are allowed to be
        // inconsistent inside the transaction and are required to be consistent
        // at its end. The transaction is left open so the caller can repair it
        // or roll it back, which is what SQLite does too.
        self.check_deferred_foreign_keys()?;
        // Every module flushes what it is holding before the log's commit
        // record, because what it flushes is more writes.
        self.sync_modules()?;
        // `PRAGMA defer_foreign_keys` is the transaction's setting, not the
        // connection's, and SQLite clears it at each commit and rollback.
        if self.defer_foreign_keys {
            self.defer_foreign_keys = false;
            self.forget_compiled_statements();
        }
        // Nothing to abandon once it is committed, and holding the before-images
        // would hold every row a long transaction touched.
        self.undo.borrow_mut().clear();
        self.marks.clear();
        let Some(txn) = self.batch.take() else {
            self.touched = 0;
            return Ok(());
        };
        let participants = std::mem::take(&mut self.touched);
        self.commit_across(txn, participants)
    }

    /// Commits one transaction across every file it wrote.
    ///
    /// **One file is the path this engine has always taken; two is a
    /// super-journal.** A transaction that wrote a single database appends one
    /// `Commit` record and waits for it, exactly as before - no marker, no extra
    /// file, no stat. A transaction that wrote two or more files that will be
    /// recovered writes a super-journal listing them, marks each one as being in
    /// doubt, appends every vote, and then deletes the super-journal. That
    /// deletion is the commit: before it, every participant recovers without the
    /// transaction; after it, every one recovers with it.
    ///
    /// A temporary database is not a participant. It has no file, so it has no
    /// recovery to be in doubt about, and including it would make an ordinary
    /// `CREATE TEMP TABLE ... INSERT` pay for a protocol that decides nothing.
    ///
    /// @param txn - the transaction to commit
    /// @param participants - the schemas it wrote
    fn commit_across(&mut self, txn: u64, participants: u16) -> DbResult<()> {
        let durable: Vec<usize> = schemas_in(participants)
            .filter(|at| self.path_of(*at).is_some())
            .collect();
        self.decided_over.set(durable.len());
        if durable.len() < 2 {
            return self.vote(txn, participants);
        }
        let files: Vec<PathBuf> = durable.iter().filter_map(|at| self.path_of(*at)).collect();
        let near = self.path.clone();
        let mut journal = multi::SuperJournal::create(&near, txn, &files)?;
        // **Every marker is durable before any vote is.** A `Commit` that
        // reached the disk while its marker had not would be replayed by a
        // recovery that never learned to doubt it, which is the one ordering
        // this protocol cannot get wrong.
        for at in &durable {
            let Some(path) = self.path_of(*at) else {
                continue;
            };
            if let Err(error) = journal.mark(&path, txn) {
                journal.abandon();
                return Err(error);
            }
        }
        if let Err(error) = self.vote(txn, participants) {
            // **Not abandoned, and that is the point.** Some participants may
            // already have their `Commit` on disk; removing the super-journal
            // would make those count and the rest not, which is precisely the
            // torn commit this protocol exists to prevent. Leaving it makes
            // every vote a vote that lost, which is the outcome that is
            // consistent. Dropping the handle removes nothing: `SuperJournal`
            // has no destructor precisely so that the safe outcome is the one a
            // path which returns early gets for free.
            drop(journal);
            return Err(error);
        }
        journal.commit()
    }

    /// Appends and awaits one `Commit` record per schema a transaction wrote.
    ///
    /// @param txn - the transaction
    /// @param participants - the schemas it wrote
    fn vote(&mut self, txn: u64, participants: u16) -> DbResult<()> {
        // A transaction that wrote nothing still commits `main`, which is what
        // an empty `BEGIN; COMMIT;` has always done and what keeps the
        // transaction numbers in step with the log.
        let mut wrote_any = false;
        for at in schemas_in(participants) {
            let Some(wal) = self.log_of(at) else {
                continue;
            };
            wal.commit(txn, txn)?;
            if let Some(database) = self.schema_file(at) {
                database.pool().set_durable_lsn(wal.write_ahead_point());
            }
            wrote_any = true;
        }
        if !wrote_any {
            self.wal.commit(txn, txn)?;
            self.database
                .pool()
                .set_durable_lsn(self.wal.write_ahead_point());
        }
        Ok(())
    }

    /// Returns how many databases the last commit was decided over.
    ///
    /// One, or none, for every transaction that wrote a single file - which is
    /// every statement the performance gate measures. Two or more is a
    /// super-journal.
    pub fn decided_over(&self) -> usize {
        self.decided_over.get()
    }

    /// Returns what the write path has done to every tree, added up.
    ///
    /// The counters, not the clock. For a write the counters are the story: a
    /// page compacted is a whole page image in the log, and a tree that
    /// compacts once per statement is doing work no timing will explain on its
    /// own.
    /// Folds every attached database's log into its own file.
    ///
    /// A checkpoint is per file, because a log is per file. `checkpoint` does
    /// `main`; this does the rest, so that a connection closed after one is not
    /// leaving an attached database's committed rows in a log the next open of
    /// *that file alone* would still have to replay.
    fn checkpoint_attached(&mut self) -> DbResult<()> {
        for nth in 0..self.attached.len() {
            let Some(held) = self.attached.get_mut(nth) else {
                continue;
            };
            if held.path.is_none() {
                // A database with no file has nothing to fold a log into.
                continue;
            }
            held.wal.sync()?;
            held.wal.roll_segment()?;
            let durable = held.wal.write_ahead_point();
            held.database.pool().set_durable_lsn(durable);
            let sequence = held.wal.sequence();
            held.database.set_log_position(durable, 0, sequence);
            held.database.checkpoint()?;
            held.wal.note_checkpoint(durable, 0)?;
            // The segments below the checkpoint describe changes the file now
            // holds, so keeping them is keeping a second copy of the database
            // for ever. See `Database::checkpoint` for the measurement.
            held.wal.retire_segments_below(durable)?;
            held.database
                .pool()
                .set_durable_lsn(held.wal.write_ahead_point());
        }
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

    /// Checks every tree's structure: key order, separators and fill.
    ///
    /// Writes a verified copy of this database into a new file.
    ///
    /// **What `VACUUM INTO` does**, and the same thing
    /// `connect::Database::backup_to` does for a caller with a handle: fold the
    /// log into the file, copy the file, then open the copy and walk it. A
    /// backup nobody checked is a file that is assumed to be a database, and
    /// the cost of finding out otherwise is paid at the worst possible moment.
    ///
    /// It is a copy rather than a page-by-page rebuild because this engine is
    /// single threaded and one file is one pool: there is no second writer to
    /// race, which is the whole reason SQLite's backup API is incremental.
    ///
    /// @param path - where the copy goes
    ///
    /// **Unreachable since `VACUUM INTO` took over producing a verified copy.**
    /// Kept rather than deleted for now; it is a candidate for removal in a
    /// later cleanup pass.
    #[allow(dead_code)]
    pub(crate) fn backup_into(&mut self, path: &std::path::Path) -> DbResult<()> {
        self.checkpoint()?;
        std::fs::copy(&self.path, path).map_err(|error| {
            inillucent_base::error::misuse(format!(
                "cannot copy {} to {}: {error}",
                self.path.display(),
                path.display()
            ))
        })?;
        let copy = ImportedDatabase::open(path.to_path_buf(), self.page_size, self.frames)?;
        copy.check_trees()
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
        crate::rebuild::rebuild_into(self, destination, self.page_size, self.frames)
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
    /// The rebuild is written beside the database and then moved over it, so a
    /// crash at any point leaves either the original or the rebuilt file whole
    /// and never a half-written one. The connection reopens onto the new file
    /// afterwards, because every tree handle it holds names a root that has
    /// moved.
    pub(crate) fn vacuum_in_place(&mut self) -> DbResult<()> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos() as u64)
            .unwrap_or(0);
        let scratch = crate::rebuild::scratch_beside(&self.path, stamp);
        let _ = std::fs::remove_file(&scratch);
        self.rebuild_into(&scratch)?;
        let path = self.path.clone();
        let page_size = self.page_size;
        let frames = self.frames;
        // **The old file is closed before it is replaced, not after.** Every
        // handle this connection holds names a root in the file that is about
        // to go, and a pool still holding frames of a file whose bytes have
        // changed underneath it is a pool that will answer from the database
        // that used to be there. Assigning through `self` is what closes it:
        // the old value is dropped as the old value of the assignment, which is
        // the only moment in this function when neither file is open by us.
        *self = ImportedDatabase::open(scratch.clone(), page_size, frames)?;
        // **And its log segments go with it.** They describe the pages of the
        // database that was there; left beside the file they would be replayed
        // over the rebuilt one on the next open, which is the rebuild undone by
        // recovery. The checkpoint at the head of `rebuild_into` already folded
        // everything they hold into the file that is about to be overwritten.
        crate::rebuild::remove_log_segments(&path);
        std::fs::copy(&scratch, &path).map_err(|error| {
            inillucent_base::error::misuse(format!(
                "cannot write the rebuilt database over {}: {error}",
                path.display()
            ))
        })?;
        *self = ImportedDatabase::open(path, page_size, frames)?;
        crate::rebuild::remove_log_segments(&scratch);
        let _ = std::fs::remove_file(&scratch);
        Ok(())
    }

    /// Checks every tree in every attached database, and their agreement.
    ///
    /// The campaign tests run this after every statement. A tree that has
    /// drifted structurally still answers a scan correctly for a long time,
    /// which is precisely why the check has to be a check rather than a query.
    pub fn check_trees(&self) -> DbResult<()> {
        for (root, tree) in &self.trees {
            let pool = self
                .schema_file(self.schema_of(*root))
                .ok_or_else(|| refusal("a tree names a database that is not attached"))?
                .pool();
            tree.check(pool)?;
        }
        // **And then whether the trees agree with each other.** Every check
        // above is about one tree in isolation - its key order, its sibling
        // chain, its separators - and every one of them passes over a database
        // where an index holds two entries under one `UNIQUE` key, or an entry
        // naming a row the table does not have. That state was reachable when
        // `UPDATE` skipped a secondary `UNIQUE` index's own check, and
        // `PRAGMA integrity_check` called it healthy - which is what this
        // detector exists for: the write path is where such a state is
        // *created*, and there is more than one way in - an import, a crash
        // recovery, a future write path, a bug like that one.
        self.check_indexes_agree()
    }

    /// Writes one entry straight into an index tree, past the write path.
    ///
    /// **A repair and diagnosis hook, and the only way to test the integrity
    /// checker.** Now that `UPDATE` enforces every `UNIQUE` index, no SQL
    /// statement can leave an index holding two entries under one `UNIQUE` key,
    /// or an entry naming a row the table does not have - which is also why a
    /// checker for those states cannot be exercised through SQL. A detector
    /// that has never been shown the damage it looks for is a detector nobody
    /// has tested.
    ///
    /// It maintains nothing and checks nothing: no uniqueness, no table row, no
    /// other index. That is deliberate and is the whole of its use. It goes
    /// through `write`, so the change is logged, committed and recoverable like
    /// any other - the damage is a real state of a real database rather than an
    /// artefact of the test harness.
    ///
    /// @param index - the index's name, as declared
    /// @param entry - the entry: the key columns, then whatever identifies the
    ///   row
    /// @param adding - true to write it, false to remove it
    pub fn write_index_entry_unchecked(
        &mut self,
        index: &str,
        entry: &[OwnedDatum],
        adding: bool,
    ) -> DbResult<()> {
        let folded = index.to_ascii_lowercase().into_bytes();
        let root = self
            .tables
            .iter()
            .flat_map(|table| table.indexes.iter())
            .find(|held| held.folded == folded)
            .map(|held| held.root)
            .ok_or_else(|| refusal(format!("no such index: {index}")))?;
        let owned: Vec<OwnedDatum> = entry.to_vec();
        self.write(&Params::new(), Vec::new(), move |target, _| {
            let (database, trees, log) = target.parts_for(root)?;
            let tree = trees
                .get_mut(root)
                .ok_or_else(|| refusal("the index has no tree"))?;
            let borrowed: Vec<Datum<'_>> = owned.iter().map(OwnedDatum::borrow).collect();
            if adding {
                tree.put(database, log, &borrowed)?;
            } else {
                tree.delete(database, log, &borrowed)?;
            }
            Ok(Changes::default())
        })?;
        Ok(())
    }

    /// Reports the first disagreement between an index and the table it is on.
    ///
    /// **Four kinds of disagreement, in SQLite's own wording**, because an
    /// application matching on `integrity_check`'s answer is matching on that
    /// text:
    ///
    /// - `non-unique entry in index <name>` - two entries under one key in a
    ///   `UNIQUE` index. The entries are in key order, so this is a comparison
    ///   against the previous entry and costs one extra comparison per entry
    ///   rather than a second pass. A prefix containing a NULL is skipped,
    ///   because SQL's rule is that every NULL is distinct - the same rule
    ///   `distinct_prefix` applies on the write path.
    /// - `row <rowid> missing from index <name>` - a table row whose entry is
    ///   not there, which is also what an entry whose *key* does not match its
    ///   row looks like from here: the recomputed key is not found.
    /// - `wrong # of entries in index <name>` - which is what an entry naming a
    ///   row the table does not hold shows up as, once every row that is there
    ///   has been found.
    ///
    /// **A partial index and an index on an expression are checked for
    /// uniqueness only.** Deciding which rows *should* have an entry means
    /// evaluating the predicate, and deciding what an entry's key should be
    /// means evaluating the key expression; both need a binder, which the
    /// checker does not have. Counting them as though every row had an entry
    /// would report a healthy partial index as damaged, which is worse than
    /// not looking.
    fn check_indexes_agree(&self) -> DbResult<()> {
        for table in &self.tables {
            let Some(layout) = self.layouts.get(&table.root) else {
                continue;
            };
            let Some(table_tree) = self.trees.get(&table.root) else {
                continue;
            };
            let Some(file) = self.schema_file(self.schema_of(table.root)) else {
                continue;
            };
            for index in &table.indexes {
                if index.root == 0 || index.root == table.root {
                    continue;
                }
                let Some(index_tree) = self.trees.get(&index.root) else {
                    continue;
                };
                let Some(index_file) = self.schema_file(self.schema_of(index.root)) else {
                    continue;
                };
                // **Two sequential walks and a merge, with no probe between
                // them.** The obvious algorithm is SQLite's - walk the table
                // and seek the index for each row - and this engine cannot
                // afford it: a descent swizzles the pointer it followed, so
                // probing a hundred thousand distinct leaves pins the pool, and
                // `an_index_build_bigger_than_the_pool_completes` reported
                // exactly that on its 64 frames. Both trees are walked left to
                // right instead, and the table's implied entries are put into
                // the index's own key order first - by `in_key_order`, which is
                // the tree's own encoding under the tree's own collations, so
                // there is no second opinion about ordering to drift from the
                // first.
                let computed = index.partial_sql.is_some()
                    || index.columns.iter().any(|key| key.expr_sql.is_some());
                let (specs, _) = index_shape(table, index, index.root);
                let width = index.columns.len();
                let mut implied: Vec<Vec<OwnedDatum>> = Vec::new();
                if !computed {
                    let mut page = table_tree.first_leaf();
                    while !page.is_none() {
                        let mut next = inillucent_pool::PageId::NONE;
                        table_tree.visit_from(file.pool(), page, &mut |leaf| {
                            next = leaf.right_sibling();
                            for row in leaf.live()? {
                                let owned: Vec<OwnedDatum> =
                                    row.iter().map(OwnedDatum::from_datum).collect();
                                implied.push(plain_index_entry(index, layout, &owned));
                            }
                            Ok(false)
                        })?;
                        page = next;
                    }
                    implied = in_key_order(implied, &specs, specs.len());
                }
                let rows = implied.len() as u64;
                let mut wanted = implied.into_iter();
                let mut missing: Option<Vec<OwnedDatum>> = None;
                let mut entries = 0u64;
                let mut previous: Option<Vec<OwnedDatum>> = None;
                index_tree.visit_leaves(index_file.pool(), &mut |leaf| {
                    for entry in leaf.live()? {
                        entries = entries.saturating_add(1);
                        let held: Vec<OwnedDatum> =
                            entry.iter().map(OwnedDatum::from_datum).collect();
                        if index.unique {
                            let key = held.get(..width).unwrap_or_default();
                            // Every NULL is distinct, so a key holding one is
                            // not a duplicate of anything - the same rule
                            // `distinct_prefix` applies on the write path.
                            if key.iter().any(|value| matches!(value, OwnedDatum::Null)) {
                                previous = None;
                            } else {
                                if previous.as_deref() == Some(key) {
                                    return Err(corrupt_index(format!(
                                        "non-unique entry in index {}",
                                        String::from_utf8_lossy(&index.name)
                                    )));
                                }
                                previous = Some(key.to_vec());
                            }
                        }
                        // **A partial index and an index on an expression are
                        // checked for uniqueness only.** Deciding which rows
                        // should have an entry means evaluating the predicate,
                        // and deciding what a key should be means evaluating
                        // the key expression; both need a binder the checker
                        // does not have, and counting them as though every row
                        // had an entry would report a healthy partial index as
                        // damaged.
                        if computed || missing.is_some() {
                            continue;
                        }
                        match wanted.next() {
                            Some(want) if want == held => {}
                            Some(want) => missing = Some(want),
                            // More entries than the table implies. The count
                            // below is what names that.
                            None => {}
                        }
                    }
                    Ok(true)
                })?;
                if computed {
                    continue;
                }
                // The row is named before the count, because a table row whose
                // entry was taken away is both - and SQLite names the row.
                if let Some(want) = missing.or_else(|| wanted.next()) {
                    return Err(corrupt_index(format!(
                        "row {} missing from index {}",
                        entry_identity_text(&want, width),
                        String::from_utf8_lossy(&index.name)
                    )));
                }
                if entries != rows {
                    return Err(corrupt_index(format!(
                        "wrong # of entries in index {}",
                        String::from_utf8_lossy(&index.name)
                    )));
                }
            }
        }
        Ok(())
    }

    /// Sets what a commit waits for.
    ///
    /// @param policy - the `synchronous` setting
    pub fn set_synchronous(&self, policy: Synchronous) {
        self.wal.set_synchronous(policy);
    }

    /// Writes every dirty page and advances the log's recovery point.
    ///
    /// The log is synced *first*, so that every page about to be written is one
    /// the log has already described durably. The other order is the durability
    /// mutant the Phase 3 gate exists to kill.
    pub fn checkpoint(&mut self) -> DbResult<()> {
        // **The catalog's statistics are made honest first, and inside the
        // transaction the checkpoint is about to make durable.** A tree's shape
        // changes on every split and every insert, and rewriting a catalog row
        // that often would put a catalog write on the write path. A checkpoint
        // is the moment it is cheap: the file is being flushed anyway, and what
        // the next open reads is the shape as of the last checkpoint - which is
        // exactly what the next open needs, because everything after it is in
        // the log for recovery to replay.
        self.refresh_statistics()?;
        self.wal.sync()?;
        // **The segment boundary is moved to the checkpoint point first.**
        // A segment is only retirable once every record in it is below the
        // checkpoint LSN, and the segment being appended to never is - the
        // checkpoint record itself lands in it. Rolling here is what turns
        // "everything except the current segment" into "everything", and it is
        // the difference between a log that shrinks and one that keeps one
        // segment's worth of a finished build for ever.
        self.wal.roll_segment()?;
        let durable = self.wal.write_ahead_point();
        self.database.pool().set_durable_lsn(durable);
        let sequence = self.wal.sequence();
        self.database.set_log_position(durable, 0, sequence);
        self.database.checkpoint()?;
        self.wal.note_checkpoint(durable, 0)?;
        // **And then the segments the checkpoint has made redundant go.**
        //
        // `retire_segments_below` was written, documented as "called after a
        // checkpoint", and covered by six cases in `inillucent-wal`'s recovery
        // tests - and called from exactly one place, `inillucent-txn`'s engine,
        // which is not the engine that ships. The consequence was measured: the
        // same 200,000 rows are 18.4 MB in SQLite and 179.1 MB here, 27.6 MB of
        // data file and 151.5 MB of log segments that survive a checkpoint, a
        // clean close, a reopen and a second checkpoint.
        //
        // It is safe to do here rather than only at close because the function
        // deletes a segment only when every record in it is below the
        // checkpoint LSN *and* the next segment starts at or below it, so a
        // segment holding anything recovery would still need is left alone -
        // and a segment it cannot unlink is left alone and reported `Ok`,
        // because failing a checkpoint over a file that would not delete would
        // turn a tidy-up into an outage.
        self.wal.retire_segments_below(durable)?;
        self.database
            .pool()
            .set_durable_lsn(self.wal.write_ahead_point());
        // Every attached database too, because a log is per file and a
        // connection closed after a checkpoint should leave databases rather
        // than databases and logs nobody will open again.
        self.checkpoint_attached()?;
        Ok(())
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
        Ok(Statement(self.compiled(sql)?))
    }

    /// Runs a statement [`ImportedDatabase::prepare_statement`] compiled.
    ///
    /// @param statement - the handle
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute_statement(
        &mut self,
        statement: &Statement,
        params: &Params,
    ) -> DbResult<Outcome> {
        let held = std::rc::Rc::clone(&statement.0);
        self.execute_compiled(&held, params)
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
        let cached = std::rc::Rc::clone(&statement.0);
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
            Cached::Insert(_, Some((plan, prepared)), _) => {
                physical::run_any_prepared(plan, self, prepared, params)?.0
            }
            Cached::Update(_, plan, prepared, _, _) | Cached::Delete(_, plan, prepared) => {
                self.keys_of(plan, prepared, params)?
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
            Cached::Select(plan, prepared) => {
                physical::run_any_prepared(plan, self, prepared, params)?;
            }
            Cached::Insert(statement, ..) => {
                self.write(params, Vec::new(), |target, params| {
                    dml::insert(statement, target, params, &rows)
                })?;
            }
            Cached::Update(statement, _, _, _, setup) => {
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

    /// Returns what the binder needs to know about the registered functions.
    ///
    /// The name, the arity and whether it reduces a group - and nothing else.
    /// A binder that held the *body* would be a bound tree that depends on who
    /// was holding it, which is why the machinery looks the body up when it
    /// runs rather than carrying it.
    fn external_functions(&self) -> Vec<inillucent_sql::function::ExternalFunction> {
        self.registry
            .functions()
            .iter()
            .map(|held| inillucent_sql::function::ExternalFunction {
                name: held.name.to_ascii_lowercase().into_bytes(),
                arity: held.arity,
                aggregate: held.is_aggregate(),
            })
            .collect()
    }

    /// Registers a scalar an application defined, replacing one of the same
    /// name and arity.
    ///
    /// @param name - the name SQL calls it by
    /// @param arity - how many arguments it takes, or -1 for any number
    /// @param flags - what the function promises about itself
    /// @param body - what it does
    pub fn create_scalar_function(
        &mut self,
        name: &str,
        arity: i32,
        flags: inillucent_ext::registry::FunctionFlags,
        body: inillucent_ext::registry::ScalarBody,
    ) -> DbResult<()> {
        self.register_function(inillucent_ext::registry::UserFunction {
            name: name.to_string(),
            arity,
            flags,
            body: inillucent_ext::registry::UserBody::Scalar(body),
        })
    }

    /// Registers an aggregate an application defined.
    ///
    /// @param name - the name SQL calls it by
    /// @param arity - how many arguments it takes, or -1 for any number
    /// @param flags - what the function promises about itself
    /// @param body - what it does with a whole group
    pub fn create_aggregate_function(
        &mut self,
        name: &str,
        arity: i32,
        flags: inillucent_ext::registry::FunctionFlags,
        body: inillucent_ext::registry::AggregateBody,
    ) -> DbResult<()> {
        self.register_function(inillucent_ext::registry::UserFunction {
            name: name.to_string(),
            arity,
            flags,
            body: inillucent_ext::registry::UserBody::Aggregate(body),
        })
    }

    /// Puts one function into the registry and forgets the compiled statements.
    ///
    /// **The cache has to go.** Which function a name resolves to is decided
    /// when a statement is bound - a registration can shadow a built-in - so a
    /// statement compiled before the registration would keep calling the
    /// built-in, and one compiled before a *removal* would keep calling code
    /// the application has taken back.
    ///
    /// @param function - the registration
    fn register_function(
        &mut self,
        function: inillucent_ext::registry::UserFunction,
    ) -> DbResult<()> {
        self.registry.register_function(function);
        self.forget_compiled_statements();
        Ok(())
    }

    /// Removes a function by name and arity, reporting whether one went.
    ///
    /// @param name - the name it was registered under
    /// @param arity - the arity it was registered for
    pub fn remove_function(&mut self, name: &str, arity: i32) -> bool {
        let removed = self.registry.unregister_function(name, arity);
        if removed {
            self.forget_compiled_statements();
        }
        removed
    }

    /// Registers a collating sequence an application defined.
    ///
    /// **The comparator is process-wide and the name is not.** A `Collation` is
    /// a `Copy` handle carried through every key and every comparison, so the
    /// body lives in `inillucent-value`'s table; what this connection holds is
    /// the name it resolves to that handle by.
    ///
    /// @param name - the name `COLLATE` calls it by
    /// @param comparator - how it orders two values
    pub fn create_collation(
        &mut self,
        name: &str,
        comparator: inillucent_value::collation::Comparator,
    ) -> DbResult<()> {
        let collation = inillucent_value::collation::register_custom(name, comparator);
        let folded = name.to_ascii_uppercase();
        self.collations.retain(|(existing, _)| *existing != folded);
        self.collations.push((folded, collation));
        // A comparison compiled under BINARY would keep comparing under BINARY.
        self.forget_compiled_statements();
        Ok(())
    }

    /// Puts the connection into or out of defensive mode.
    ///
    /// @param on - whether the flag is in force
    pub fn set_defensive(&mut self, on: bool) {
        self.defensive = on;
    }

    /// Installs the authorizer every later statement is bound under.
    ///
    /// **The plan cache is emptied with it**, for the same reason it is emptied
    /// when a lever changes: a plan compiled under one authorizer is that
    /// authorizer's answer, and reusing it would skip the callback the caller
    /// installed the authorizer to receive.
    ///
    /// @param authorizer - the callback, or nothing to allow everything again
    pub fn set_authorizer(
        &mut self,
        authorizer: Option<std::rc::Rc<dyn inillucent_sql::bind::Authorizer>>,
    ) {
        self.authorizer = authorizer;
        self.statements.borrow_mut().clear();
    }

    /// Turns off one or more planner optimizations for this connection.
    ///
    /// **Every compiled statement goes with it.** A plan built under a lever is
    /// that lever's answer, and re-running it after the lever changed would
    /// measure the old choice while reporting the new one - which is the whole
    /// thing a lever exists to compare.
    ///
    /// @param mask - the levers to switch off
    pub fn disable_optimizations(&mut self, mask: u32) {
        // **The cache is keyed by the levers rather than cleared by them.** A
        // plan built under a lever is that lever's answer, so the same SQL under
        // two settings is two entries; clearing would make the second arm's
        // first execution pay a compile the first arm's did not, and that
        // difference is the size of the thing such a measurement looks for.
        self.levers = Levers::without(self.levers.disabled() | mask);
    }

    /// Returns which planner optimizations this connection has on.
    pub fn levers(&self) -> Levers {
        self.levers
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
        let journal = mode.is_rollback().then(|| {
            inillucent_pool::journal::Journal::new(
                held,
                &DbPath::new(self.path.to_string_lossy().as_ref()),
                mode,
                self.page_size,
            )
        });
        self.database.pool().set_journal(journal);
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
    /// A `WHERE` that is a rowid equality is answered from the plan itself -
    /// see `physical::rowid_seek_key` - and everything else runs the query. The
    /// row may not exist, and that is not this function's problem: the write
    /// path reads each key before it changes anything and skips the ones that
    /// are not there.
    ///
    /// @param plan - the keys query
    /// @param prepared - its structural choice
    /// @param params - the bound parameters
    fn keys_of(
        &self,
        plan: &PhysicalPlan,
        prepared: &physical::Prepared,
        params: &Params,
    ) -> DbResult<Vec<Vec<OwnedDatum>>> {
        if let Some(key) = physical::rowid_seek_key(plan, params)? {
            return Ok(vec![vec![key]]);
        }
        Ok(physical::run_any_prepared(plan, self, prepared, params)?.0)
    }

    /// Runs one already-compiled statement.
    ///
    /// @param cached - the compiled statement
    /// @param params - the bound parameters
    fn execute_compiled(
        &mut self,
        cached: &std::rc::Rc<Cached>,
        params: &Params,
    ) -> DbResult<Outcome> {
        // **The connection's counters, handed to the statement before it is
        // compiled.** `changes()`, `total_changes()`, `last_insert_rowid()` and
        // the random built-ins' seed are questions about the connection rather
        // than about a row, and the executor has no connection - so they are
        // read once here and travel on the parameter set, which is the same
        // route a folded subquery takes and for the same reason: a plan is
        // cached by its text, and a value baked into the plan would answer with
        // whatever was true when it was first compiled.
        params.set_context(self.scalar_context());
        params.set_recursive_triggers(self.recursive_triggers);
        // **The file lock, taken here and released here.** Both entry points -
        // `execute_any` and `execute_statement` - come through this function, so
        // no statement can run without it. Under `exclusive`, which is the
        // default, both calls compare two integers. Under `normal` this is what
        // lets a second process have the file between statements, and what makes
        // this connection notice when one has written to it.
        self.enter(Self::writes_of(cached))?;
        let outcome = self.apply_compiled(cached, params);
        self.leave()?;
        let outcome = outcome?;
        // **The cyclic half of a foreign key's action happens here**, after the
        // statement rather than inside it, because a cascade that can reach
        // itself cannot be inlined to a depth the data decides: the body would
        // have to appear once per level the data happens to be deep, and that
        // is not known when the statement is compiled.
        //
        // It sits on this function rather than on `execute_any` because this is
        // the funnel *both* callers reach - a statement run by text and a
        // statement prepared and stepped - and a settle that only one of them
        // performed would leave the tree half-repaired depending on which API
        // the application happened to use.
        if !self.settling.get() {
            self.settling.set(true);
            let settled = self.settle_foreign_keys();
            self.settling.set(false);
            settled?;
        }
        Ok(outcome)
    }

    /// Runs one already-compiled statement, without settling anything after it.
    ///
    /// @param cached - the compiled statement
    /// @param params - the bound parameters
    fn apply_compiled(
        &mut self,
        cached: &std::rc::Rc<Cached>,
        params: &Params,
    ) -> DbResult<Outcome> {
        // **`PRAGMA query_only` is enforced here, at the one place every
        // compiled statement passes through.** A caller sets it to make a
        // mistake impossible, so recording it and writing anyway would be worse
        // than not having the pragma at all. The message is SQLite's own, which
        // is what an application's error handling is written against.
        if self.query_only && writes_something(cached) {
            return Err(inillucent_base::error::DbError::primary(
                inillucent_base::error::PrimaryCode::ReadOnly,
            )
            .with_message("attempt to write a readonly database")
            .with_detail("attempt to write a readonly database"));
        }
        match &**cached {
            // **Borrowed, not cloned.** `cached` is an `Rc` the caller already
            // holds, so the statement outlives anything this does to `self` -
            // including the DDL path emptying the plan cache. Cloning it was a
            // whole bound statement copied per execution, and for a module
            // insert that is once per row.
            Cached::Nothing => Ok(Outcome::empty()),
            Cached::Ddl(sql) => self.execute_ddl(sql),
            Cached::QueryPlan(lines) => Ok(query_plan_rows(lines)),
            Cached::Program(rows) => Ok(program_rows(rows)),
            Cached::VirtualInsert(statement) => self.insert_into_module(statement, params),
            Cached::Select(plan, prepared) => {
                let (rows, shape) = physical::run_any_prepared(plan, self, prepared, params)?;
                Ok(Outcome {
                    rows,
                    names: names_of(&shape),
                    changes: Changes::default(),
                })
            }
            Cached::Insert(statement, source, values_hold_subquery) => {
                let rows = match source {
                    Some((plan, prepared)) => {
                        physical::run_any_prepared(plan, self, prepared, params)?.0
                    }
                    None => Vec::new(),
                };
                // A `VALUES` list has expressions and no plan, so the
                // plan-shaped fold never sees it. Folded here instead, or a
                // subquery in a value would be refused as though it were
                // correlated - which is what an unfilled slot looks like from
                // inside the physical pass. The flag was decided when the
                // statement was compiled: an insert that holds no subquery is
                // the common case and pays nothing for this.
                let folded = if *values_hold_subquery {
                    self.fold_values(statement, params)?
                } else {
                    None
                };
                let params = folded.as_ref().unwrap_or(params);
                self.write(
                    params,
                    returning_names(&statement.returning),
                    |target, params| dml::insert(statement, target, params, &rows),
                )
            }
            Cached::Update(statement, plan, prepared, assignments_hold_subquery, setup) => {
                let keys = self.keys_of(plan, prepared, params)?;
                // The same for an `UPDATE`'s assignments: the plan above finds
                // the rows, and the values written into them are evaluated by
                // the write path from expressions the plan never carried.
                let folded = if *assignments_hold_subquery || !statement.returning.is_empty() {
                    let assigned: Vec<&inillucent_sql::bind::BoundExpr> = statement
                        .assignments
                        .iter()
                        .map(|assignment| &assignment.value)
                        .chain(statement.returning.iter().map(|column| &column.expr))
                        .collect();
                    inillucent_exec::subquery::fold_expressions(&assigned, self, params)?
                } else {
                    None
                };
                let params = folded.as_ref().unwrap_or(params);
                self.write(
                    params,
                    returning_names(&statement.returning),
                    |target, params| dml::update_cached(statement, target, params, &keys, setup),
                )
            }
            Cached::VirtualUpdate(statement, plan, prepared) => {
                let keys = physical::run_any_prepared(plan, self, prepared, params)?.0;
                let changed = self.update_module(statement, &keys, params)?;
                if self.batch.get().is_none() {
                    self.sync_modules()?;
                    self.seal()?;
                }
                Ok(Outcome {
                    rows: Vec::new(),
                    names: Vec::new(),
                    changes: Changes {
                        rows: changed,
                        ..Default::default()
                    },
                })
            }
            Cached::VirtualDelete(statement, plan, prepared) => {
                let keys = physical::run_any_prepared(plan, self, prepared, params)?.0;
                let mut changed = 0usize;
                for key in &keys {
                    let Some(rowid) = key.first() else { continue };
                    self.change_module(
                        &statement.table.name,
                        &inillucent_sql::vtab::Change::Delete(inillucent_exec::scalar::to_value(
                            rowid.borrow(),
                        )),
                    )?;
                    changed = changed.saturating_add(1);
                }
                if self.batch.get().is_none() {
                    self.sync_modules()?;
                    self.seal()?;
                }
                Ok(Outcome {
                    rows: Vec::new(),
                    names: Vec::new(),
                    changes: Changes {
                        rows: changed,
                        ..Default::default()
                    },
                })
            }
            Cached::Delete(statement, plan, prepared) => {
                let keys = self.keys_of(plan, prepared, params)?;
                // A `RETURNING` clause is a result-column list the write path
                // evaluates directly, so the plan-shaped fold never sees its
                // subqueries. `DELETE ... RETURNING id, (SELECT count(*) FROM
                // b)` came back as "a correlated subquery" - which is what an
                // unfilled slot looks like from inside `translate`, and a true
                // sentence about the slot rather than about the query.
                let returned: Vec<&inillucent_sql::bind::BoundExpr> = statement
                    .returning
                    .iter()
                    .map(|column| &column.expr)
                    .collect();
                let folded = inillucent_exec::subquery::fold_expressions(&returned, self, params)?;
                let params = folded.as_ref().unwrap_or(params);
                self.write(
                    params,
                    returning_names(&statement.returning),
                    |target, params| dml::delete(statement, target, params, &keys),
                )
            }
        }
    }

    /// Folds the subqueries in an insert's `VALUES` list, when it has one.
    ///
    /// An insert whose source is a `SELECT` is planned, so its subqueries are
    /// folded by the plan-shaped path along with everything else in that plan.
    /// A `VALUES` list is not planned at all - the write path evaluates its
    /// expressions directly - so it is folded here.
    ///
    /// @param statement - the bound insert
    /// @param params - the values bound for this execution
    fn fold_values(
        &self,
        statement: &inillucent_sql::dml::BoundInsert,
        params: &Params,
    ) -> DbResult<Option<Params>> {
        let inillucent_sql::dml::BoundInsertSource::Values(rows) = &statement.source else {
            return Ok(None);
        };
        let values: Vec<&inillucent_sql::bind::BoundExpr> = rows.iter().flatten().collect();
        inillucent_exec::subquery::fold_expressions(&values, self, params)
    }

    /// Compiles an `EXPLAIN`, which the two forms of do different things.
    ///
    /// **`EXPLAIN QUERY PLAN` is answerable and plain `EXPLAIN` is not**, and
    /// the reason is structural rather than unfinished. SQLite's `EXPLAIN`
    /// lists the opcodes of the bytecode program it compiled; this engine
    /// compiles no bytecode - it builds an operator chain - so there is no
    /// opcode listing to print, and printing the operator chain under that name
    /// would be answering a different question with the same word.
    ///
    /// `EXPLAIN QUERY PLAN` asks what the plan *is*, which this engine can
    /// answer: `Prepared::describe` already renders the chain, and the
    /// benchmark harness has been printing it beside SQLite's since Phase 1 so
    /// a reader can see whether the two chose the same structure.
    ///
    /// @param sql - the whole statement text, for a refusal to quote
    /// @param query_plan - whether `QUERY PLAN` was written
    /// @param inner - the statement being explained
    /// @param parsed - the parse the statement came out of
    fn compile_explain(
        &self,
        sql: &str,
        query_plan: bool,
        inner: &inillucent_sql::ast::Statement,
        parsed: &inillucent_sql::parser::ParsedStatement,
    ) -> DbResult<Cached> {
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
            .with_foreign_keys(self.foreign_keys, self.defer_foreign_keys);
        let bound = binder.bind_statement(inner).map_err(refused)?;
        let lines = match bound {
            BoundStatement::Select(select) => plan_select_with(*select, self.levers).describe(),
            // A write's plan is the query that finds the rows it changes, and
            // that is the thing a reader is asking about - "did my DELETE use
            // the index" is the same question as "did the search use it".
            // Answering "a delete" would be answering that it is a delete,
            // which the reader wrote.
            BoundStatement::Update(statement) => self
                .keys_plan(
                    &statement.table,
                    statement.source,
                    statement.filter.as_ref(),
                    statement.limit.as_ref(),
                    statement.offset.as_ref(),
                )?
                .0
                .describe(),
            BoundStatement::Delete(statement) => self
                .keys_plan(
                    &statement.table,
                    statement.source,
                    statement.filter.as_ref(),
                    statement.limit.as_ref(),
                    statement.offset.as_ref(),
                )?
                .0
                .describe(),
            other => vec![describe_statement(&other).to_string()],
        };
        if query_plan {
            return Ok(Cached::QueryPlan(lines));
        }
        Ok(Cached::Program(program_of(&lines)))
    }

    /// Compiles one statement as far as its parameters allow.
    ///
    /// @param sql - the statement text
    fn compile(&self, sql: &str) -> DbResult<Cached> {
        // Counted here rather than at the three call sites, so a fourth path to
        // a compilation cannot be added without moving this number with it.
        self.compiles.set(self.compiles.get().saturating_add(1));
        // `EXPLAIN` is decided before binding, because the binder's job is the
        // statement being explained and not the explaining. The old engine did
        // this a level up, where a VDBE program was available to render; here
        // there is no program, and that difference is the whole of the
        // `query_plan` split below.
        let parsed = self.parse_once(sql)?;
        if let inillucent_sql::ast::Statement::Explain { query_plan, inner } = &parsed.statement {
            return self.compile_explain(sql, *query_plan, inner, &parsed);
        }
        let bound = self.bind_parsed(sql, &parsed);
        self.recycle(parsed);
        match bound? {
            BoundStatement::Select(select) => {
                let plan = plan_select_with(*select, self.levers);
                let prepared = physical::prepare_any(&plan, self)?;
                Ok(Cached::Select(Box::new(plan), Box::new(prepared)))
            }
            BoundStatement::Insert(statement)
                if statement.table.kind == inillucent_sql::catalog_view::TableKind::Virtual =>
            {
                // A write to a virtual table is the *module's* to make. The
                // engine evaluates the row and hands it over; what happens to it
                // is the module's business, which is what makes a module a
                // module rather than a table with a funny name.
                Ok(Cached::VirtualInsert(statement))
            }
            BoundStatement::Insert(statement) => {
                let source = match &statement.source {
                    inillucent_sql::dml::BoundInsertSource::Select(select) => {
                        let plan = plan_select_with((**select).clone(), self.levers);
                        let prepared = physical::prepare_any(&plan, self)?;
                        Some((Box::new(plan), Box::new(prepared)))
                    }
                    inillucent_sql::dml::BoundInsertSource::Values(_) => None,
                };
                let values_hold_subquery = match &statement.source {
                    inillucent_sql::dml::BoundInsertSource::Values(rows) => rows
                        .iter()
                        .flatten()
                        .any(inillucent_sql::plan::expression_holds_subquery),
                    inillucent_sql::dml::BoundInsertSource::Select(_) => false,
                };
                Ok(Cached::Insert(statement, source, values_hold_subquery))
            }
            // **A write to a virtual table is the module's to make**, the same
            // way an insert and a delete already are. `UPDATE f SET body=...`
            // reached the ordinary path, asked for the layout of a table with
            // no tree, and answered "no layout imported for the table being
            // written" - so an fts5 table could be inserted into and deleted
            // from and never corrected.
            BoundStatement::Update(statement)
                if statement.table.kind == inillucent_sql::catalog_view::TableKind::Virtual =>
            {
                let select = inillucent_exec::dml::module_keys_query(
                    &statement.table,
                    statement.source,
                    statement.filter.as_ref(),
                    statement.limit.as_ref(),
                    statement.offset.as_ref(),
                );
                let plan = plan_select_with(select, self.levers);
                let prepared = physical::prepare_any(&plan, self)?;
                Ok(Cached::VirtualUpdate(
                    statement,
                    Box::new(plan),
                    Box::new(prepared),
                ))
            }
            BoundStatement::Update(statement) => {
                let (plan, prepared) = self.update_keys_plan(&statement)?;
                let assignments_hold_subquery = statement.assignments.iter().any(|assignment| {
                    inillucent_sql::plan::expression_holds_subquery(&assignment.value)
                });
                Ok(Cached::Update(
                    statement,
                    Box::new(plan),
                    Box::new(prepared),
                    assignments_hold_subquery,
                    dml::UpdateCache::default(),
                ))
            }
            BoundStatement::Delete(statement)
                if statement.table.kind == inillucent_sql::catalog_view::TableKind::Virtual =>
            {
                let select = inillucent_exec::dml::module_keys_query(
                    &statement.table,
                    statement.source,
                    statement.filter.as_ref(),
                    statement.limit.as_ref(),
                    statement.offset.as_ref(),
                );
                let plan = plan_select_with(select, self.levers);
                let prepared = physical::prepare_any(&plan, self)?;
                Ok(Cached::VirtualDelete(
                    statement,
                    Box::new(plan),
                    Box::new(prepared),
                ))
            }
            BoundStatement::Delete(statement) if statement.view_rows.is_some() => {
                let rows = statement
                    .view_rows
                    .as_ref()
                    .ok_or_else(|| refusal("a view delete with no query"))?;
                let plan = plan_select_with((**rows).clone(), self.levers);
                let prepared = physical::prepare_any(&plan, self)?;
                Ok(Cached::Delete(
                    statement,
                    Box::new(plan),
                    Box::new(prepared),
                ))
            }
            BoundStatement::Delete(statement) => {
                let (plan, prepared) = self.keys_plan(
                    &statement.table,
                    statement.source,
                    statement.filter.as_ref(),
                    statement.limit.as_ref(),
                    statement.offset.as_ref(),
                )?;
                Ok(Cached::Delete(
                    statement,
                    Box::new(plan),
                    Box::new(prepared),
                ))
            }
            // A directive is *not* cached as a compiled thing: it changes the
            // catalog the next statement will be bound against, and the whole
            // point of `refresh_catalog` is that what was compiled before a DDL
            // statement is not run after it. So the entry holds the text, and
            // the execution re-binds against the schema as it is at that
            // moment.
            BoundStatement::Directive(_) => Ok(Cached::Ddl(sql.to_string())),
            // **Text that is only a comment is a statement that does nothing,
            // not a statement that cannot be run.** A `-- comment` after the
            // last `;` of a script is the ordinary way to end a file, and it
            // was refused with "`-- trailing` binds to nothing, which the new
            // engine does not run yet" - which stops the script rather than the
            // comment. SQLite runs it as a no-op and so does this.
            BoundStatement::Empty => Ok(Cached::Nothing),
        }
    }

    /// Plans and prepares the query that finds the rows a write will change.
    ///
    /// @param table - the table being written
    /// @param source - the statement-wide number of its FROM term
    /// @param filter - the statement's `WHERE`
    /// @param limit - the statement's `LIMIT`
    /// @param offset - the statement's `OFFSET`
    fn keys_plan(
        &self,
        table: &TableInfo,
        source: usize,
        filter: Option<&inillucent_sql::bind::BoundExpr>,
        limit: Option<&inillucent_sql::bind::BoundExpr>,
        offset: Option<&inillucent_sql::bind::BoundExpr>,
    ) -> DbResult<(PhysicalPlan, physical::Prepared)> {
        let layout = self
            .layouts
            .get(&table.root)
            .ok_or_else(|| refusal("no layout imported for the table being written"))?;
        let select = dml::keys_query(table, source, filter, limit, offset, layout)?;
        let plan = plan_select_with(select, self.levers);
        let prepared = physical::prepare_any(&plan, self)?;
        Ok((plan, prepared))
    }

    /// Returns the query that finds an `UPDATE`'s rows, and its shape.
    ///
    /// For an ordinary `UPDATE` this is [`ImportedDatabase::keys_plan`]. For an
    /// `UPDATE ... FROM` the query also carries the joined terms and projects
    /// the assigned values beside the key, because those values read a row the
    /// write path never sees.
    ///
    /// @param statement - the bound update
    fn update_keys_plan(
        &self,
        statement: &inillucent_sql::dml::BoundUpdate,
    ) -> DbResult<(PhysicalPlan, physical::Prepared)> {
        // **A view's `keys` are its rows.** There is no tree to find a key in:
        // an `INSTEAD OF UPDATE` fires with `OLD` taken from running the view,
        // which is exactly the query the binder left on `view_rows`.
        if let Some(rows) = &statement.view_rows {
            let plan = plan_select_with((**rows).clone(), self.levers);
            let prepared = physical::prepare_any(&plan, self)?;
            return Ok((plan, prepared));
        }
        if statement.from.is_empty() {
            return self.keys_plan(
                &statement.table,
                statement.source,
                statement.filter.as_ref(),
                statement.limit.as_ref(),
                statement.offset.as_ref(),
            );
        }
        let layout = self
            .layouts
            .get(&statement.table.root)
            .ok_or_else(|| refusal("no layout imported for the table being written"))?;
        let assigned: Vec<inillucent_sql::bind::BoundExpr> = statement
            .assignments
            .iter()
            .map(|assignment| assignment.value.clone())
            .collect();
        let select = dml::keys_query_joined(
            &statement.table,
            statement.source,
            statement.filter.as_ref(),
            statement.limit.as_ref(),
            statement.offset.as_ref(),
            layout,
            &statement.from,
            &assigned,
        )?;
        let plan = plan_select_with(select, self.levers);
        let prepared = physical::prepare_any(&plan, self)?;
        Ok((plan, prepared))
    }

    /// Runs one write as its own transaction, logged and committed.
    ///
    /// The commit record is appended and awaited *after* the change, which is
    /// what makes the change atomic: recovery replays a transaction only if it
    /// found the commit, so a crash anywhere inside `apply` leaves a log that
    /// describes nothing that happened.
    ///
    /// @param params - the bound parameters
    /// @param apply - what to change
    fn write(
        &mut self,
        params: &Params,
        names: Vec<String>,
        apply: impl FnOnce(&mut dyn WriteTarget, &Params) -> DbResult<Changes>,
    ) -> DbResult<Outcome> {
        // A statement inside an open batch joins it and does not commit; a
        // statement outside one is its own transaction and does.
        let (txn, autocommit) = match self.batch.get() {
            Some(held) => (held, false),
            None => {
                let txn = self.next_txn.get();
                self.next_txn.set(txn.saturating_add(1));
                (txn, true)
            }
        };
        // **Collected whether or not a transaction is open**, because a
        // statement is abandoned by more than a rollback. SQLite's default
        // algorithm is `ABORT`, which undoes *the statement* and keeps the
        // transaction, and an autocommit statement gets it too: this buffer
        // used to be `None` outside a transaction on the reasoning that
        // "an autocommit statement cannot be abandoned", and that was the bug -
        // a four-row `INSERT` failing on its third row kept the first two and
        // committed them.
        //
        // Outside a transaction it holds at most one statement: `write` clears
        // it when the statement ends, either way. Every schema's log appends to
        // the one buffer, because a rollback undoes one *transaction* rather
        // than one file - and each record carries the schema it came out of.
        let undo = Some(&self.undo);
        // **Where this statement's writes begin.** The success path does
        // nothing with it; the failure path rolls back to it. That asymmetry is
        // the whole cost of statement atomicity inside a transaction - one
        // integer read off a `Vec`'s length - which is why there is no
        // per-statement savepoint here and `txn.large`'s two thousand
        // statements do not pay for two thousand of them.
        let mark = self.undo.borrow().len();
        let main_log = WalLog {
            wal: std::rc::Rc::clone(&self.wal),
            txn,
            schema: MAIN,
            wrote: false,
            undo,
        };
        let logs = if self.attached.is_empty() && self.temps.is_empty() {
            Logs::One(main_log)
        } else {
            // One per schema *number*, so that `logs[at]` is the log of the file
            // schema `at` names. The temporary slot is filled with `main`'s log
            // when this session has no temporary database, and nothing can reach
            // it: a handle that resolved to `TEMP` could only have come from a
            // temporary tree, which only exists when the schema does.
            let mut held: Vec<WalLog<'_>> =
                Vec::with_capacity(self.attached.len().saturating_add(2));
            held.push(main_log);
            held.push(match self.schema_at(TEMP) {
                Some(temp) => WalLog {
                    wal: std::rc::Rc::clone(&temp.wal),
                    txn,
                    schema: TEMP,
                    wrote: false,
                    undo,
                },
                None => WalLog {
                    wal: std::rc::Rc::clone(&self.wal),
                    txn,
                    schema: MAIN,
                    wrote: false,
                    undo,
                },
            });
            for (nth, attached) in self.attached.iter().enumerate() {
                held.push(WalLog {
                    wal: std::rc::Rc::clone(&attached.wal),
                    txn,
                    schema: FIRST_ATTACHED.saturating_add(nth),
                    wrote: false,
                    undo,
                });
            }
            Logs::Many(held)
        };
        let session = self.session.get();
        let (applied, wrote, counted) = {
            let mut view = WriteView {
                database: &mut self.database,
                attached: &mut self.attached,
                temps: &mut self.temps,
                session,
                logs,
                owner: &self.owner,
                trees: &mut self.trees,
                layouts: &self.layouts,
                covering: &self.covering,
                indexed: &self.vector_indexes,
                counted: std::cell::Cell::new((0, 0, None)),
            };
            // **Not `?`.** A failed statement has writes of its own to put
            // back, and the borrow of the trees has to end before anything can.
            let applied = apply(&mut view, params);
            // **The participant set, read off the logs that were used.** A
            // transaction that wrote one file commits the way it always has; one
            // that wrote two is decided by a super-journal, and this is the only
            // place that can tell them apart without asking every tree. Read on
            // the failure path too, because a statement that failed partway
            // still wrote, and `OR FAIL` keeps what it wrote.
            let wrote = view.logs.wrote();
            // Read on the failure path too, and for the same reason `wrote` is:
            // a statement that failed partway still wrote, and `OR FAIL` keeps
            // it - so the counters have to see it.
            let counted = view.rows_written();
            (applied, wrote, counted)
        };
        let changes = match applied {
            Ok(changes) => changes,
            Err(error) => {
                // **The rowid moves even though the row does not.** SQLite
                // documents `last_insert_rowid()` as the last rowid
                // *attempted*, and measures out that way: an `INSERT` of two
                // rows that fails on the second answers the first row's rowid,
                // with the table holding neither.
                self.remember_rowid(counted.2);
                // What the statement kept, which is what the counters count.
                // `FAIL` keeps the rows it wrote and everything else puts them
                // back, so the tally is taken only for `FAIL` - which is what
                // makes `UPDATE OR FAIL` report `1 | 4` and a plain `UPDATE`
                // that aborts report `0` and no movement at all.
                if error.unwind() == Unwind::Nothing {
                    self.record_changes(counted.0, counted.1);
                } else {
                    self.last_changes.set(0);
                }
                return Err(self.abandon(error, mark, autocommit, wrote, txn));
            }
        };
        self.touched |= wrote;
        // **Inside the same transaction, and after the trees rather than
        // during them.** The module is registered on the connection and the
        // write borrowed the connection apart, so this is the first moment both
        // halves exist at once. Doing it before the commit below is what makes
        // the table and the index it carries one change rather than two.
        //
        // **It fails the way the statement fails**, not past it: the index it
        // maintains is part of the write, so a statement that could not
        // maintain it is a statement that did not happen. Reached with the
        // trees no longer borrowed, which is what lets it undo.
        if let Err(error) = self.follow_vector_indexes(&changes) {
            return Err(self.abandon(error, mark, autocommit, wrote, txn));
        }
        if autocommit {
            // **Nothing else can abandon what an autocommit statement wrote**,
            // so the before-images stop being useful here rather than growing
            // for the life of the connection. Held until now so that everything
            // above can still be undone, and cleared before the commit so that
            // a commit which fails leaves nothing behind for the next
            // statement's mark to sit on top of.
            self.undo.borrow_mut().clear();
        }
        self.remember_rowid(changes.last_rowid);
        // **Read off the view rather than off `Changes`**, so the success path
        // and the failure path count the same way and a trigger's rows land in
        // `total_changes()` where SQLite puts them.
        self.record_changes(counted.0, counted.1);
        if autocommit {
            let participants = std::mem::take(&mut self.touched);
            self.commit_across(txn, participants)?;
        }
        Ok(Outcome {
            rows: changes.returned.clone(),
            names,
            changes,
        })
    }

    /// Puts back what a failed statement wrote, as its algorithm says.
    ///
    /// **The three raising algorithms differ only here.** `ABORT` - which is
    /// what an error carrying no algorithm at all reads as, and so what a
    /// `STRICT` type failure, a foreign key and a trigger's `RAISE` get -
    /// undoes back to the mark `write` took and leaves the transaction open.
    /// `ROLLBACK` continues up to the transaction, which is the existing
    /// `rollback` in full: floor zero, the savepoints gone, the batch closed.
    /// `FAIL` undoes nothing, and is the only one this engine already matched.
    ///
    /// The error it returns is the statement's own unless the undo itself
    /// failed, in which case that failure is the one worth reporting: a
    /// constraint message describing a database that is now in a state nobody
    /// intended is worse than saying so.
    ///
    /// @param error - what the statement failed with
    /// @param mark - the undo buffer's length before the statement wrote
    /// @param autocommit - whether the statement was its own transaction
    /// @param wrote - the schemas the statement wrote, as a participant set
    /// @param txn - the transaction the statement wrote under
    fn abandon(
        &mut self,
        error: DbError,
        mark: usize,
        autocommit: bool,
        wrote: u16,
        txn: u64,
    ) -> DbError {
        let unwind = error.unwind();
        let undone = match unwind {
            Unwind::Nothing => Ok(()),
            Unwind::Statement => self.undo_to_floor(mark, false, txn),
            // **In autocommit the two are the same thing**: the statement is
            // the transaction, so `ROLLBACK` is `ABORT` with a floor of zero,
            // and there is no batch to close. Inside one it is the existing
            // `rollback` in full - the savepoints gone, the batch closed, the
            // schema refreshed.
            Unwind::Transaction if autocommit => self.undo_to_floor(0, false, txn),
            Unwind::Transaction => self.rollback(),
        };
        if autocommit {
            self.undo.borrow_mut().clear();
            self.marks.clear();
            if matches!(unwind, Unwind::Nothing) && undone.is_ok() {
                // **`OR FAIL` outside a transaction commits.** The rows written
                // before the failure are kept, and keeping them only in the
                // page cache would make them a fact this process believes and
                // the file does not. The statement failed; its transaction did
                // not.
                self.touched |= wrote;
                let participants = std::mem::take(&mut self.touched);
                if let Err(failure) = self.commit_across(txn, participants) {
                    return failure;
                }
            } else {
                // Undone, so there is nothing to commit and nothing to name as
                // a participant. The restores are logged like any other write
                // and no `Commit` follows them, so a recovery replays neither
                // the statement nor its undo.
                self.touched = 0;
            }
        }
        undone.err().unwrap_or(error)
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

/// Returns a shape's column names as strings.
///
/// @param shape - what the built plan produces
/// Returns the names a `RETURNING` clause's columns report.
///
/// **A write that answers rows has to name them.** `INSERT ... RETURNING a, b`
/// produced its rows and an empty name list, so a caller drawing a grid had two
/// columns of values and no headings for them - which `inillucent-driver`'s
/// conformance suite caught, because a result with rows and no columns is a
/// shape nothing else in the engine produces.
///
/// The names are the binder's own `BoundResultColumn::name`, which is where a
/// `SELECT`'s come from too, so `SELECT a` and `INSERT ... RETURNING a` cannot
/// disagree about what the column is called.
///
/// @param returning - the bound `RETURNING` columns
fn returning_names(returning: &[inillucent_sql::bind::BoundResultColumn]) -> Vec<String> {
    returning
        .iter()
        .map(|column| String::from_utf8_lossy(&column.name).into_owned())
        .collect()
}

/// Returns the names a plan's result columns report.
fn names_of(shape: &physical::Shape) -> Vec<String> {
    shape
        .names
        .iter()
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect()
}

/// Renders `EXPLAIN QUERY PLAN` lines as the rows a caller reads.
///
/// The four columns are SQLite's - `id`, `parent`, `notused`, `detail` - so a
/// caller written against SQLite reads the same shape and finds its text where
/// it expects it. The ids are the line's position rather than a tree: this
/// engine's `describe` renders the chain source-first as a list, and inventing
/// a parent for each line would be inventing structure the renderer does not
/// carry. SQLite documents its own `EXPLAIN QUERY PLAN` output as unstable
/// between releases, so the text was never the comparable part.
///
/// @param lines - the plan's operators, source first
/// Returns the listing a plain `EXPLAIN` answers with.
///
/// **The eight columns SQLite answers with, holding this engine's steps.**
/// `EXPLAIN` in SQLite lists the opcodes of a bytecode program; this engine
/// compiles no bytecode, so what is listed is the operator chain the statement
/// actually runs - one row per stage, framed by the `Init` and `Halt` that
/// begin and end every execution here as they do there.
///
/// The columns are used for what they mean rather than left at zero: `p1` is
/// the step's position in the chain, `p2` is where control goes next, `p4`
/// carries the operator's argument, and `comment` is the same sentence
/// `EXPLAIN QUERY PLAN` prints. A reader comparing two engines' listings is
/// comparing two different machines and will see that; a reader asking what
/// *this* statement does gets an answer rather than a refusal.
///
/// @param lines - the plan, as `EXPLAIN QUERY PLAN` describes it
fn program_of(lines: &[String]) -> Vec<(String, i64, i64, String, String)> {
    let mut program = Vec::with_capacity(lines.len().saturating_add(2));
    let last = lines.len().saturating_add(1) as i64;
    program.push((
        "Init".to_string(),
        0,
        1,
        String::new(),
        "Start at 1".to_string(),
    ));
    for (at, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start_matches(['`', '-', '|', ' ']);
        let (word, rest) = match trimmed.split_once(' ') {
            Some((word, rest)) => (word, rest),
            None => (trimmed, ""),
        };
        program.push((
            opcode_name(word),
            at as i64,
            at.saturating_add(2) as i64,
            rest.to_string(),
            trimmed.to_string(),
        ));
    }
    program.push(("Halt".to_string(), 0, 0, String::new(), String::new()));
    let _ = last;
    program
}

/// Returns a plan word as an opcode name.
///
/// `SCAN` and `SEARCH` are the two the planner writes most, and the rest are
/// title-cased so that a listing reads as a program rather than as a shouted
/// sentence.
///
/// @param word - the first word of the plan line
fn opcode_name(word: &str) -> String {
    let mut name = String::with_capacity(word.len());
    for (at, letter) in word.chars().enumerate() {
        if at == 0 {
            name.extend(letter.to_uppercase());
        } else {
            name.extend(letter.to_lowercase());
        }
    }
    name
}

/// Returns the rows a plain `EXPLAIN` answers with.
///
/// @param program - the steps, as `program_of` built them
fn program_rows(program: &[(String, i64, i64, String, String)]) -> Outcome {
    Outcome {
        rows: program
            .iter()
            .enumerate()
            .map(|(address, (opcode, one, two, argument, comment))| {
                vec![
                    OwnedDatum::Int(address as i64),
                    OwnedDatum::Text(opcode.as_bytes().to_vec()),
                    OwnedDatum::Int(*one),
                    OwnedDatum::Int(*two),
                    OwnedDatum::Int(0),
                    OwnedDatum::Text(argument.as_bytes().to_vec()),
                    OwnedDatum::Int(0),
                    OwnedDatum::Text(comment.as_bytes().to_vec()),
                ]
            })
            .collect(),
        names: vec![
            "addr".to_string(),
            "opcode".to_string(),
            "p1".to_string(),
            "p2".to_string(),
            "p3".to_string(),
            "p4".to_string(),
            "p5".to_string(),
            "comment".to_string(),
        ],
        changes: Changes::default(),
    }
}

/// Adds every object a bound query reads to a list.
///
/// Recursive through subqueries, because a table a subquery reads is a table
/// the statement uses - which is the question `tables_used` answers.
///
/// @param select - the bound query
/// @param into - the list being built
fn collect_sources(
    select: &inillucent_sql::bind::BoundSelect,
    into: &mut Vec<(&'static str, Vec<u8>)>,
) {
    for source in &select.sources {
        let kind = match source.table.kind {
            inillucent_sql::catalog_view::TableKind::View => "view",
            _ => "table",
        };
        into.push((kind, source.table.name.clone()));
    }
}

fn query_plan_rows(lines: &[String]) -> Outcome {
    Outcome {
        rows: lines
            .iter()
            .enumerate()
            .map(|(position, line)| {
                vec![
                    OwnedDatum::Int(position as i64),
                    OwnedDatum::Int(0),
                    OwnedDatum::Int(0),
                    OwnedDatum::Text(line.as_bytes().to_vec()),
                ]
            })
            .collect(),
        names: vec![
            "id".to_string(),
            "parent".to_string(),
            "notused".to_string(),
            "detail".to_string(),
        ],
        changes: Changes::default(),
    }
}

/// A statement compiled once and run many times.
///
/// Opaque on purpose: what is inside is the engine's business, and a caller that
/// could see it would be a caller that could be broken by a plan shape changing.
pub struct Statement(std::rc::Rc<Cached>);

/// One statement, compiled as far as it can be before its parameters arrive.
///
/// A `SELECT` is a plan and the structural choice over it. A write is the bound
/// statement plus, for an `UPDATE` or a `DELETE`, the plan that finds the rows
/// it will change - which is an ordinary query and is prepared like one, so
/// `WHERE id = ?1` reaches the same point probe on the second execution as on
/// the first.
enum Cached {
    /// Text that carries no statement at all.
    ///
    /// A `-- comment` after the last `;`, an empty string, whitespace. Running
    /// it produces no rows and changes nothing, which is what SQLite does with
    /// the same text.
    Nothing,
    /// A statement the session carries out itself, held as its own text.
    ///
    /// Re-bound on every execution, because binding a `DROP TABLE` resolves
    /// whether the table is there and the answer changes when it runs.
    Ddl(String),
    /// `EXPLAIN QUERY PLAN`, rendered when the statement was compiled.
    ///
    /// The lines describe the plan and the plan depends on the schema, so this
    /// is cached and invalidated exactly like the query it describes - which is
    /// the point of holding it here rather than rendering it per execution.
    QueryPlan(Vec<String>),
    /// A plain `EXPLAIN`, rendered when the statement was compiled.
    ///
    /// One entry per step: the opcode's name, its first two operands, its
    /// argument, and the comment. See `program_of` for what those mean here.
    Program(Vec<(String, i64, i64, String, String)>),
    /// An insert into a virtual table, which the module applies.
    VirtualInsert(Box<inillucent_sql::dml::BoundInsert>),
    /// A delete from a virtual table, with the query that finds its rowids.
    ///
    /// A module owns its storage, so the only handle on one of its rows is the
    /// rowid it answers with: the plan asks which rowids match and the module
    /// is told about each. That is what SQLite does, and the reason `xUpdate`
    /// takes a rowid rather than a predicate.
    VirtualDelete(
        Box<inillucent_sql::dml::BoundDelete>,
        Box<PhysicalPlan>,
        Box<physical::Prepared>,
    ),
    /// An update of a virtual table, with the query that finds its rowids.
    ///
    /// The same shape as [`Cached::VirtualDelete`] and for the same reason: a
    /// module owns its storage, so the only handle on one of its rows is the
    /// rowid it answers with.
    VirtualUpdate(
        Box<inillucent_sql::dml::BoundUpdate>,
        Box<PhysicalPlan>,
        Box<physical::Prepared>,
    ),
    /// A query.
    Select(Box<PhysicalPlan>, Box<physical::Prepared>),
    /// An insert, with the plan for its `SELECT` source when it has one.
    ///
    /// The flag says whether a `VALUES` list holds a subquery. It is decided
    /// once, here, because the alternative is walking the value expressions on
    /// every execution of every insert - and `BoundExpr::children` allocates a
    /// vector per node, which is the cost this project already measured on the
    /// read path at about 0.07 us per execution.
    Insert(
        Box<inillucent_sql::dml::BoundInsert>,
        Option<(Box<PhysicalPlan>, Box<physical::Prepared>)>,
        bool,
    ),
    /// An update, with the plan that finds the rows it changes.
    ///
    /// The flag says whether an assignment holds a subquery, for the reason
    /// above.
    Update(
        Box<inillucent_sql::dml::BoundUpdate>,
        Box<PhysicalPlan>,
        Box<physical::Prepared>,
        bool,
        /// Everything the statement builds before it looks at a row, kept
        /// between executions. See `dml::UpdateSetup`: it was more than half of
        /// what `txn.large` cost, and none of it depends on the row.
        dml::UpdateCache,
    ),
    /// A delete, with the plan that finds the rows it removes.
    Delete(
        Box<inillucent_sql::dml::BoundDelete>,
        Box<PhysicalPlan>,
        Box<physical::Prepared>,
    ),
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
    // A parse or bind refusal is caller-safe by construction: it names tables,
    // columns and constructs, which are the caller's own words, and never a
    // path, a bound value or page bytes. The detail is left in place so that
    // everything reading it - the shell, the gate, the surface inventory -
    // sees exactly what it saw before.
    let mut built = refusal(error.message()).with_message(error.message());
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
}

impl TreeLog for WalLog<'_> {
    fn log(&mut self, body: Body<'_>) -> DbResult<u64> {
        self.wrote = true;
        self.wal.append(self.txn, body)
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
    pool.on_log_behind(Box::new(move || {
        held.sync()?;
        Ok(held.write_ahead_point())
    }));
}

/// A [`RowRedo`] that learns a tree's shape from the catalog rows it replays.
///
/// **The problem it solves.** `TreeRows` has to be told every tree's column
/// directory before the replay starts, and the only place to get one is the
/// catalog. But the catalog a reader can read before the replay is the catalog
/// as at the *last checkpoint* - so a table created after it, and then written
/// to, names a tree the applier has never heard of, and the replay refuses.
/// That is not a corner: a database that is created, given a schema and filled
/// without ever being checkpointed is the ordinary shape of a crash, and it is
/// exactly what `ImportedDatabase::create` followed by DDL produces.
///
/// **Why it works.** A `CREATE TABLE` is a row inserted into the catalog tree,
/// and the log is replayed in LSN order - so that row goes past *before* any row
/// of the tree it describes. Watching the catalog tree go by is therefore enough
/// to know every shape by the time it is needed, and it needs no second pass.
///
/// The catalog row is decoded by `inillucent-catalog`'s own decoder rather than
/// here, because a second decoder is a second opinion about which column is
/// which, and the columns are what the format is.
///
/// ## `seen` is a catalog, not a list of the rows that went past
///
/// It used to be the second, and that made a database unopenable after an
/// ordinary migration. `ALTER TABLE chunk ADD COLUMN embedded_at INTEGER`
/// rewrites the table's catalog row - a `DeleteRow` and an `InsertRow` - so a
/// list that only appends held **both** definitions, and `shape_of` reads the
/// owner table with `find`, which answers with the first. Every shape derived
/// after that ALTER therefore came from the definition before it.
///
/// What that costs is not a missing column in a report. `CREATE INDEX
/// chunk_embedded_at_idx ON chunk (embedded_at)` against a table with no
/// `embedded_at` resolves the key to no column at all, and `index_shape` gives
/// an unresolved key column `PhysicalType::Any` where the writer used
/// `PhysicalType::Int64`. An `Any` mini-column is wider, so recovery repacks the
/// index's leaves less densely than the process that wrote them - and the first
/// logical `CompactLeaf` replayed against such a leaf cannot fit rows that
/// demonstrably fitted when they were written. The open fails with
/// `database disk image is malformed`, and the whole database is unreachable
/// while the log is beside it.
///
/// It was measured on Nikaya's real 5.8 GB corpus: `chunk`'s catalog row was
/// rewritten at LSN 21,934,260,592 and the index's row written at
/// 21,936,659,792, both after the last checkpoint at 21,074,969,552; recovery
/// then refused a compaction of index leaf 237505 that held 2,031 live rows.
/// Replaying that page's records out of the parked log with the shape read off
/// the page fits every one of them, and with the first key column forced to
/// `Any` it fails at the same LSN with the same 2,031 rows.
///
/// So a row is *remembered* rather than appended: an entry replaces the one it
/// supersedes, by name and by tree identifier, and every shape is derived again
/// from the catalog as it now stands. Deriving them all again rather than only
/// the one that changed is what makes an ALTER reach the indexes on the table -
/// their own rows may have gone past already, and their shapes come from the
/// table's text rather than from their own.
struct LearningRows {
    /// The applier this delegates to, gaining trees as it goes.
    rows: TreeRows,
    /// The catalog as it now stands: at most one entry per object.
    seen: Vec<SchemaEntry>,
}

impl LearningRows {
    /// Returns an applier that already knows the checkpointed catalog.
    ///
    /// @param checkpointed - the catalog as at the last checkpoint
    fn new(checkpointed: &[SchemaEntry]) -> LearningRows {
        let mut learning = LearningRows {
            rows: TreeRows::new().with_tree(
                inillucent_catalog::paged::SCHEMA_TREE_ID,
                schema_layout(),
                1,
            ),
            seen: checkpointed.to_vec(),
        };
        learning.derive_every_shape();
        learning
    }

    /// Learns a tree's shape from a catalog row the replay is about to apply.
    ///
    /// Silent about a row it cannot make a shape of - a view, a trigger, an
    /// index whose table has not gone past yet - because the applier refuses by
    /// name if a record then needs it, and refusing there says which tree.
    ///
    /// @param row - the catalog row's encoded values
    fn learn(&mut self, row: &[u8]) {
        let Ok(values) = decode_row(row) else {
            return;
        };
        let Ok(entry) = inillucent_catalog::paged::entry_from_row(&values) else {
            return;
        };
        self.remember(entry);
        self.derive_every_shape();
    }

    /// Puts one catalog entry in place of the one it supersedes.
    ///
    /// Matched on the folded name **and** on the tree identifier: an
    /// `ALTER TABLE ... ADD COLUMN` rewrites the row under the same name, and an
    /// `ALTER TABLE ... RENAME TO` rewrites it under a new name with the same
    /// identifier. Both leave one entry behind, which is what the rest of this
    /// type assumes.
    ///
    /// @param entry - the entry the replay just read
    fn remember(&mut self, entry: SchemaEntry) {
        let name = entry.name.to_ascii_lowercase();
        let kind = entry.kind;
        let identifier = entry.tree_id;
        self.seen.retain(|held| {
            let same_name = held.kind == kind && held.name.to_ascii_lowercase() == name;
            let same_tree = identifier != 0 && held.tree_id == identifier;
            !same_name && !same_tree
        });
        self.seen.push(entry);
    }

    /// Derives every tree's shape again from the catalog as it now stands.
    ///
    /// Every one of them, not only the entry that changed: an index's columns
    /// come from its *table's* declaration, so a table whose row was just
    /// rewritten changes the shape of indexes whose own rows went past earlier.
    fn derive_every_shape(&mut self) {
        let mut rows = std::mem::take(&mut self.rows);
        for entry in &self.seen {
            let Ok(identifier) = identifier_of(entry) else {
                continue;
            };
            let Some((columns, key_columns)) = shape_of(entry, &self.seen, identifier) else {
                continue;
            };
            rows = rows.with_tree(u64::from(identifier), columns, key_columns);
        }
        self.rows = rows;
    }
}

/// Decodes a run of tagged values, which is how a row record carries a row.
///
/// @param row - the record's row bytes
fn decode_row(row: &[u8]) -> DbResult<Vec<Datum<'_>>> {
    let mut values = Vec::new();
    let mut at = 0usize;
    while at < row.len() {
        let (value, width) = Datum::decode_tagged(row.get(at..).unwrap_or(&[]))?;
        values.push(value);
        at = at.saturating_add(width);
    }
    Ok(values)
}

impl RowRedo for LearningRows {
    fn insert_row(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        row: &[u8],
        lsn: u64,
    ) -> DbResult<()> {
        if tree == inillucent_catalog::paged::SCHEMA_TREE_ID {
            self.learn(row);
        }
        self.rows.insert_row(database, tree, page, row, lsn)
    }

    fn delete_row(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        key: &[u8],
        lsn: u64,
    ) -> DbResult<()> {
        self.rows.delete_row(database, tree, page, key, lsn)
    }

    fn update_in_place(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        key: &[u8],
        column: u32,
        value: &[u8],
        lsn: u64,
    ) -> DbResult<()> {
        self.rows
            .update_in_place(database, tree, page, key, column, value, lsn)
    }

    fn compact_leaf(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        lsn: u64,
        from_lsn: u64,
    ) -> DbResult<()> {
        self.rows.compact_leaf(database, tree, page, lsn, from_lsn)
    }
}

/// Returns a catalog entry's tree shape, for the recovery applier.
///
/// The same derivations the open uses below, in the one form the applier wants:
/// the column directory and how many leading columns form the key. An entry
/// whose declaration will not parse - or an index whose table is not in the
/// catalog - answers `None`, and the applier then refuses any record naming it
/// rather than replaying into a shape it guessed.
///
/// @param entry - the catalog row
/// @param catalog - every row, so an index can find its table
/// @param identifier - the tree's identifier
fn shape_of(
    entry: &SchemaEntry,
    catalog: &[SchemaEntry],
    identifier: u32,
) -> Option<(Vec<ColumnSpec>, usize)> {
    match entry.kind {
        ObjectKind::Table => {
            let mut info = table_from_create_sql(&entry.sql, 0, identifier).ok()?;
            info.root = identifier;
            if info.without_rowid {
                let (columns, key_columns, _) = keyed_table_shape(&info).ok()?;
                Some((columns, key_columns))
            } else {
                let (columns, _) = table_shape(&info);
                Some((columns, 1))
            }
        }
        ObjectKind::Index => {
            let folded = entry.table.to_ascii_lowercase();
            // **The newest matching entry, not the first.** `LearningRows`
            // keeps one entry per object, so there is only ever one - but a
            // caller that hands this a catalog holding a superseded definition
            // as well should get the definition that superseded it, because the
            // shape derived from the older one is what made a database
            // unopenable after an `ALTER TABLE`.
            let owner = catalog.iter().rev().find(|held| {
                held.kind == ObjectKind::Table && held.name.to_ascii_lowercase() == folded
            })?;
            let mut table = table_from_create_sql(&owner.sql, 0, owner.tree_id as u32).ok()?;
            table.root = owner.tree_id as u32;
            // **An automatic index is declared by the *table's* text.** The
            // catalog stores an empty `sql` for one - which is what SQLite
            // writes for `sqlite_autoindex_t_1` - so parsing that empty text as
            // a `CREATE INDEX` answers nothing, the applier is told no shape,
            // and every log record naming the index is refused. That is a
            // database with a `TEXT PRIMARY KEY` that cannot be reopened after
            // a write, and it is what this branch exists to prevent; the same
            // rule is applied by `ImportedDatabase::shape_of` when the schema
            // is loaded.
            let index = if entry.sql.is_empty() {
                let folded = entry.name.to_ascii_lowercase();
                table
                    .indexes
                    .iter()
                    .find(|index| index.folded == folded)?
                    .clone()
            } else {
                inillucent_catalog::load::index_from_create_sql(&entry.sql, &table, identifier)
                    .ok()?
            };
            let (columns, _) = index_shape(&table, &index, identifier);
            let key_columns = columns.len();
            Some((columns, key_columns))
        }
        _ => None,
    }
}

/// Returns the identifier a catalog row registers its tree under.
///
/// **Refused rather than defaulted.** A zero here is a row written before the
/// identifier was persisted, and guessing one would put the tree back in the
/// state this change exists to leave: a number the writer did not use, which
/// recovery would follow to the wrong tree. A file that does not say is a file
/// this engine will not open.
///
/// @param entry - the catalog row
fn identifier_of(entry: &SchemaEntry) -> DbResult<u32> {
    if entry.tree_id == 0 {
        return Err(refusal(format!(
            "the catalog row for {} carries no tree identifier; the database was created before catalog rows carried one and has to be rebuilt",
            String::from_utf8_lossy(&entry.name)
        )));
    }
    u32::try_from(entry.tree_id).map_err(|_| {
        refusal(format!(
            "the catalog row for {} carries a tree identifier that does not fit",
            String::from_utf8_lossy(&entry.name)
        ))
    })
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
/// The old engine adds it at the connection for exactly that reason
/// (`inillucent-session`'s `connect`), and this is the same decision at the same
/// place in the new one: a database is the first thing that both builds a
/// registry and is allowed to know the retrieval engine exists.
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

/// Returns how a table's stored rows map onto the columns a query sees.
///
/// **One derivation, exposed rather than copied.** The import turns SQLite's
/// storage shape into the engine's - dropping the rowid-alias record field,
/// putting the rowid in the key column, reordering a `WITHOUT ROWID` table's
/// record into declared order - and `inillucent-migrate` has to perform the
/// identical transform to compare a source table against a migrated one. A
/// second implementation of it in the migration tool is the exact shape of bug
/// this workspace keeps paying for: two readers that agree until the first
/// table with a primary key declared after another column.
///
/// @param info - the table's declaration, as the catalog loader parsed it
pub fn source_layout_of(info: &TableInfo) -> DbResult<SourceLayout> {
    if info.without_rowid {
        Ok(keyed_table_shape(info)?.2)
    } else {
        Ok(table_shape(info).1)
    }
}

/// Returns one stored row as the columns a `SELECT *` produces.
///
/// The stored row is what `inillucent-sqlite-reader` hands back: for a rowid
/// table `[rowid] ++ record fields`, with the alias field NULL because SQLite
/// keeps the rowid in the cell key rather than in the record; for a
/// `WITHOUT ROWID` table, the record in SQLite's own field order.
///
/// @param info - the table's declaration
/// @param layout - the layout `source_layout_of` returned for it
/// @param stored - one row as the reader produced it
pub fn logical_row(
    info: &TableInfo,
    layout: &SourceLayout,
    stored: &[OwnedDatum],
) -> Vec<OwnedDatum> {
    // The tree row first, which is the shape the import builds.
    let tree: Vec<OwnedDatum> = if info.without_rowid {
        stored.to_vec()
    } else {
        let alias = info.rowid_alias.map(usize::from);
        let mut out = Vec::with_capacity(layout.width);
        out.push(stored.first().cloned().unwrap_or(OwnedDatum::Null));
        for (field, declared) in stored_positions(info).iter().enumerate() {
            if Some(*declared) == alias {
                continue;
            }
            out.push(
                stored
                    .get(field.saturating_add(1))
                    .cloned()
                    .unwrap_or(OwnedDatum::Null),
            );
        }
        out
    };
    // Then the declared order, which is what a query sees. `slots` is the map
    // the physical pass reads a column through, so using it here is using the
    // same answer.
    layout
        .slots
        .iter()
        .enumerate()
        .map(|(declared, slot)| {
            let value = slot
                .and_then(|index| tree.get(index).cloned())
                .unwrap_or(OwnedDatum::Null);
            let physical = info
                .columns
                .get(declared)
                .map(|column| physical_for(column.affinity).0)
                .unwrap_or(PhysicalType::Any);
            stored_as(physical, value)
        })
        .collect()
}

/// Returns what a column of a given layout hands back for a value put into it.
///
/// **One conversion, and it is the dialect's.** `leaf::classify_at` excepts
/// every mismatched value into the heap and returns it unchanged, with a single
/// deliberate exception: an integer in a column whose affinity is REAL is
/// *converted*, because that is what REAL affinity means - SQLite stores 7 in a
/// REAL column as 7.0 - and because excepting it would take such a column off
/// the vectorised path one whole-numbered row at a time.
///
/// So a migration that read `Int(7)` out of a SQLite record and compared it
/// against the `Real(7.0)` the new engine hands back would call a correct copy
/// wrong. This is that one rule, written where the comparison needs it.
///
/// It is guarded by measurement rather than by comment: the migration
/// acceptance runs every SQLite feature-parity fixture, and its digests fail
/// the moment this and `classify_at` disagree about any value in any of them.
///
/// @param physical - the column's layout
/// @param value - the value as the source held it
pub fn stored_as(physical: PhysicalType, value: OwnedDatum) -> OwnedDatum {
    match (physical, &value) {
        (PhysicalType::Float64, OwnedDatum::Int(number)) => OwnedDatum::Real(*number as f64),
        _ => value,
    }
}

/// Imports one table into a rowid-clustered PAX tree.
///
/// @param database - the file the tree is built in
/// @param file - the open fixture
/// @param info - the table's catalog entry
fn import_table(
    database: &mut Database,
    file: &mut SqliteFile,
    info: &TableInfo,
) -> DbResult<(TreeShape, SourceLayout)> {
    // The record's width is the count of *stored* columns, not of declared
    // ones: SQLite writes no field for a `VIRTUAL` generated column.
    let positions = stored_positions(info);
    let raw = file.read_table(info.root, positions.len())?;
    // `read_table` returns [rowid] ++ record slots. The rowid alias slot holds
    // NULL in every SQLite record, so it is dropped and the rowid takes its
    // place as the tree's key column.
    let alias = info.rowid_alias.map(usize::from);
    let (columns, layout) = table_shape(info);
    let width = layout.width;

    let mut rows: Vec<Vec<OwnedDatum>> = Vec::with_capacity(raw.len());
    for row in raw {
        let mut out: Vec<OwnedDatum> = Vec::with_capacity(width);
        out.push(row.first().cloned().unwrap_or(OwnedDatum::Null));
        for (field, declared) in positions.iter().enumerate() {
            if Some(*declared) == alias {
                continue;
            }
            out.push(
                row.get(field.saturating_add(1))
                    .cloned()
                    .unwrap_or(OwnedDatum::Null),
            );
        }
        rows.push(out);
    }

    let borrowed: Vec<Vec<Datum<'_>>> = rows
        .iter()
        .map(|row| row.iter().map(OwnedDatum::borrow).collect())
        .collect();
    let tree = PagedTree::bulk_build(
        database,
        u64::from(info.root),
        columns.clone(),
        1,
        &borrowed,
    )?;
    Ok((
        TreeShape {
            root: tree.root(),
            columns,
            key_columns: 1,
            first_leaf: tree.first_leaf(),
            leaf_count: tree.leaf_count(),
            row_count: tree.row_count(),
        },
        layout,
    ))
}

/// Returns the column directory and the layout a rowid table's tree has.
///
/// **Derived from the declaration alone**, which is what lets `CREATE TABLE`
/// and the fixture import agree by construction rather than by two people
/// writing the same rule twice. The import supplies rows read out of a SQLite
/// file and the DDL path supplies none; neither supplies a shape.
///
/// The physical type of each tree column comes from the declared affinity, and
/// that is a claim rather than a guarantee - a column declared `INTEGER` may
/// hold a string - which is exactly what the leaf's exception class is for.
///
/// @param info - the table's declaration
fn table_shape(info: &TableInfo) -> (Vec<ColumnSpec>, SourceLayout) {
    let alias = info.rowid_alias.map(usize::from);
    // `slots` is indexed by *declared* position and `None` means the tree does
    // not carry that column, which is exactly what a `VIRTUAL` generated column
    // is: it takes no record field and no tree column, and every column
    // declared after one therefore sits that many places earlier in the tree.
    let mut slots: Vec<Option<usize>> = vec![None; info.columns.len()];
    let mut next = 1usize;
    let mut columns = vec![ColumnSpec::key(PhysicalType::Int64)];
    let mut types = vec![StaticType::Int];
    for declared in stored_positions(info) {
        if Some(declared) == alias {
            if let Some(slot) = slots.get_mut(declared) {
                *slot = Some(0);
            }
            continue;
        }
        let (physical, static_type) = match info.columns.get(declared) {
            Some(column) => physical_for(column.affinity),
            None => (PhysicalType::Any, StaticType::Unknown),
        };
        columns.push(
            ColumnSpec::new(physical).with_collation(
                info.columns
                    .get(declared)
                    .map(|column| collation_of(&column.collation))
                    .unwrap_or(Collation::Binary),
            ),
        );
        types.push(static_type);
        if let Some(slot) = slots.get_mut(declared) {
            *slot = Some(next);
        }
        next = next.saturating_add(1);
    }
    let width = next;
    (
        columns,
        SourceLayout {
            tree_key: info.root,
            slots,
            rowid: Some(0),
            // A rowid table's row is identified by its rowid, and by nothing
            // else - which is what makes an index entry over one a single
            // trailing column.
            identity: vec![0],
            types,
            width,
            // A rowid-clustered tree is ordered by its rowid, which is column 0.
            key_columns: vec![0],
        },
    )
}

/// Imports one `WITHOUT ROWID` table into a key-ordered PAX tree.
///
/// A `WITHOUT ROWID` table *is* an index b-tree: there is no separate table
/// b-tree and no rowid, and the record holds every column with the primary key's
/// columns first. So the import is the index import with the whole record as the
/// row and the primary key as the key - and the resulting tree needs nothing the
/// engine does not already do, because an index tree has a multi-column key too.
///
/// **The field order is SQLite's, not the declaration's.** For
/// `CREATE TABLE t(a, b, PRIMARY KEY(b))` the record is `(b, a)`, and
/// `primary_key_position` is what says so. Reconstructing that order by guessing
/// - assuming the key is a prefix of the declared columns, say - would read the
/// right bytes into the wrong columns on any table whose primary key is not
/// written first, and every value would still be a plausible value.
///
/// @param database - the file the tree is built in
/// @param file - the open fixture
/// @param info - the table's catalog entry
fn import_keyed_table(
    database: &mut Database,
    file: &mut SqliteFile,
    info: &TableInfo,
) -> DbResult<(TreeShape, SourceLayout)> {
    let (columns, key_columns, layout) = keyed_table_shape(info)?;
    // The record's width is the count of *stored* columns: SQLite writes no
    // field for a `VIRTUAL` generated column here either.
    let rows = file.read_index(info.root, layout.width)?;
    let rows = in_key_order(rows, &columns, key_columns);
    let borrowed: Vec<Vec<Datum<'_>>> = rows
        .iter()
        .map(|row| row.iter().map(OwnedDatum::borrow).collect())
        .collect();
    let tree = PagedTree::bulk_build(
        database,
        u64::from(info.root),
        columns.clone(),
        key_columns,
        &borrowed,
    )?;
    Ok((
        TreeShape {
            root: tree.root(),
            columns,
            key_columns,
            first_leaf: tree.first_leaf(),
            leaf_count: tree.leaf_count(),
            row_count: tree.row_count(),
        },
        layout,
    ))
}

/// Returns the column directory, key width and layout of a `WITHOUT ROWID`
/// table's tree.
///
/// **The field order is SQLite's, not the declaration's.** For
/// `CREATE TABLE t(a, b, PRIMARY KEY(b))` the record is `(b, a)`, and
/// `primary_key_position` is what says so.
///
/// @param info - the table's declaration
fn keyed_table_shape(info: &TableInfo) -> DbResult<(Vec<ColumnSpec>, usize, SourceLayout)> {
    let width = info.columns.len();
    // The record's field order: primary-key columns in their key order, then
    // every other column in declaration order.
    let mut order: Vec<usize> = Vec::with_capacity(width);
    let mut keyed: Vec<(u16, usize)> = info
        .columns
        .iter()
        .enumerate()
        .filter_map(|(slot, column)| column.primary_key_position.map(|at| (at, slot)))
        .collect();
    keyed.sort_unstable();
    let key_columns = keyed.len();
    if key_columns == 0 {
        return Err(refusal(
            "a WITHOUT ROWID table with no primary key cannot be keyed",
        ));
    }
    order.extend(keyed.iter().map(|(_, slot)| *slot));
    // A `VIRTUAL` generated column is in no record and so in no tree column,
    // here for the same reason it is in none of a rowid table's - and SQLite
    // will not let one be part of a primary key, so the filter is only needed
    // over the columns that follow the key.
    for slot in 0..width {
        if !order.contains(&slot) && !is_virtual_column(info, slot) {
            order.push(slot);
        }
    }
    let stored_width = order.len();
    let mut columns = Vec::with_capacity(stored_width);
    let mut types = Vec::with_capacity(stored_width);
    // `slots[declared] = tree column`, which is the inverse of `order`.
    let mut slots: Vec<Option<usize>> = vec![None; width];
    for (position, declared) in order.iter().enumerate() {
        let (physical, static_type) = match info.columns.get(*declared) {
            Some(column) => physical_for(column.affinity),
            None => (PhysicalType::Any, StaticType::Unknown),
        };
        let collation = info
            .columns
            .get(*declared)
            .map(|column| collation_of(&column.collation))
            .unwrap_or(Collation::Binary);
        let spec = if position < key_columns {
            ColumnSpec::key(physical)
        } else {
            ColumnSpec::new(physical)
        };
        columns.push(spec.with_collation(collation));
        types.push(static_type);
        if let Some(slot) = slots.get_mut(*declared) {
            *slot = Some(position);
        }
    }
    // A non-binary collation means the tree is *seekable* but not "already
    // sorted" for an ORDER BY that did not name the same collation, which is
    // the same disqualification `index_shape` makes and for the same reason.
    let ordered = columns
        .iter()
        .take(key_columns)
        .all(|spec| spec.collation == Collation::Binary);
    Ok((
        columns,
        key_columns,
        SourceLayout {
            tree_key: info.root,
            slots,
            // There is no rowid: that is what `WITHOUT ROWID` means, and a
            // query that asks for one is refused rather than given the key.
            rowid: None,
            // The primary key is what identifies the row instead, and it is the
            // leading `key_columns` of the record. Stated here unconditionally
            // rather than read off `key_columns`, which is emptied when the
            // tree is not "already sorted" and would leave a `DESC` or collated
            // primary key with no identity at all.
            identity: (0..key_columns).collect(),
            types,
            width: stored_width,
            key_columns: if ordered {
                (0..key_columns).collect()
            } else {
                Vec::new()
            },
        },
    ))
}

/// Imports one index into a key-ordered PAX tree.
///
/// An index entry is the indexed columns followed by the rowid, which is
/// already the new format's index-tree row shape, so nothing is rearranged.
///
/// @param database - the file the tree is built in
/// @param file - the open fixture
/// @param table - the indexed table's catalog entry
/// @param index - the index's catalog entry
/// @param root - the index's root page
fn import_index(
    database: &mut Database,
    file: &mut SqliteFile,
    table: &TableInfo,
    index: &IndexInfo,
    root: u32,
) -> DbResult<(TreeShape, SourceLayout)> {
    let key_columns = index.columns.len().saturating_add(1);
    let rows = file.read_index(root, key_columns)?;
    let (columns, layout) = index_shape(table, index, root);
    let rows = in_key_order(rows, &columns, key_columns);
    let borrowed: Vec<Vec<Datum<'_>>> = rows
        .iter()
        .map(|row| row.iter().map(OwnedDatum::borrow).collect())
        .collect();
    let tree = PagedTree::bulk_build(
        database,
        u64::from(root),
        columns.clone(),
        key_columns,
        &borrowed,
    )?;
    Ok((
        TreeShape {
            root: tree.root(),
            columns,
            key_columns,
            first_leaf: tree.first_leaf(),
            leaf_count: tree.leaf_count(),
            row_count: tree.row_count(),
        },
        layout,
    ))
}

/// Returns a starting point for a connection's `random()` stream.
///
/// The wall clock and the process id. Neither is a secret and neither has to
/// be: SQLite's own `random()` is not a cryptographic generator either, and
/// what this exists to avoid is two connections - or two runs - answering the
/// same sequence. A clock that has not moved since the last open still gives a
/// different stream, because the process id is in it.
fn fresh_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|held| held.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (u64::from(std::process::id()).wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// Builds the entry an index should hold for one table row.
///
/// Only for an index whose keys are plain columns, which is what the caller has
/// already established: a computed key would need the binder to evaluate.
///
/// @param index - the index
/// @param layout - the table tree's layout
/// @param row - the row, in tree-column order
fn plain_index_entry(
    index: &IndexInfo,
    layout: &SourceLayout,
    row: &[OwnedDatum],
) -> Vec<OwnedDatum> {
    let trailing = layout.identity.len().max(1);
    let mut entry = Vec::with_capacity(index.columns.len().saturating_add(trailing));
    for column in &index.columns {
        entry.push(
            column
                .column
                .and_then(|declared| layout.slots.get(usize::from(declared)).copied().flatten())
                .and_then(|slot| row.get(slot).cloned())
                .unwrap_or(OwnedDatum::Null),
        );
    }
    if layout.identity.is_empty() {
        entry.push(
            layout
                .rowid
                .and_then(|slot| row.get(slot).cloned())
                .unwrap_or(OwnedDatum::Null),
        );
        return entry;
    }
    for slot in &layout.identity {
        entry.push(row.get(*slot).cloned().unwrap_or(OwnedDatum::Null));
    }
    entry
}

/// Names a row the way `integrity_check`'s message does, from its index entry.
///
/// An entry is the indexed columns and then whatever identifies the row: the
/// rowid for an ordinary table, and the primary key's columns for a
/// `WITHOUT ROWID` one, which has no rowid to be named by.
///
/// @param entry - the entry the table implied
/// @param width - how many leading columns are the index's own key
fn entry_identity_text(entry: &[OwnedDatum], width: usize) -> String {
    let identity = entry.get(width..).unwrap_or_default();
    if identity.is_empty() {
        return "?".to_string();
    }
    identity
        .iter()
        .map(|value| match value {
            OwnedDatum::Int(number) => number.to_string(),
            OwnedDatum::Text(text) => String::from_utf8_lossy(text).into_owned(),
            _ => "?".to_string(),
        })
        .collect::<Vec<String>>()
        .join(",")
}

/// Returns the error `integrity_check` reports a disagreement through.
///
/// A corruption rather than a refusal, because that is what it is - and the
/// pragma prints the detail, so the text SQLite would have printed is the
/// detail rather than the message.
///
/// @param said - what SQLite's own checker would say
fn corrupt_index(said: String) -> DbError {
    inillucent_base::error::corrupt(said.clone()).with_detail(said)
}

/// Returns the column directory and the layout an index tree is built with.
///
/// Shared by the fixture import, which fills the tree from SQLite's own index
/// pages, and by `CREATE INDEX`, which fills it from the table tree. The shape
/// is the same question in both cases and is answered in one place.
///
/// **What follows the indexed columns is whatever identifies the table row**:
/// a rowid for an ordinary table, and the *primary key's columns* for a
/// `WITHOUT ROWID` one, which is SQLite's rule and the reason such an index was
/// refused here until now. `SourceLayout::identity` names them, so a
/// non-covering seek probes the table with the right key without having to
/// re-derive which columns those were.
///
/// @param table - the table the index is on
/// @param index - the index's declaration
/// @param root - the identifier the tree is registered under
fn index_shape(table: &TableInfo, index: &IndexInfo, root: u32) -> (Vec<ColumnSpec>, SourceLayout) {
    let trailing = identity_columns(table);
    let key_columns = index.columns.len().saturating_add(trailing.len().max(1));
    let mut columns = Vec::with_capacity(key_columns);
    let mut types = Vec::with_capacity(key_columns);
    let mut slots: Vec<Option<usize>> = vec![None; table.columns.len()];
    let mut ordered = true;
    for (position, column) in index.columns.iter().enumerate() {
        // An expression key indexes no table column, so nothing maps onto it
        // and a query that reads the underlying column cannot be answered from
        // this tree. Leaving the slot unmapped is what makes that a refusal in
        // the physical pass rather than a wrong answer here.
        // **A `DESC` key column is stored descending**, which is what SQLite
        // stores and what makes the two engines read a `DESC` index in the same
        // order - the trailing rowid stays ascending, so ties inside a
        // descending column come out ascending in both. It used to be flattened
        // to ascending, and the visible cost was that `ORDER BY k`
        // over a `DESC` index answered its ties in the opposite order to
        // SQLite's, on every one of the seven statements `ordering.rs` names.
        let Some(declared) = column.column.map(usize::from) else {
            columns.push(ColumnSpec::key(PhysicalType::Any).with_descending(column.descending));
            types.push(StaticType::Unknown);
            continue;
        };
        let (physical, static_type) = match table.columns.get(declared) {
            Some(info) => physical_for(info.affinity),
            None => (PhysicalType::Any, StaticType::Unknown),
        };
        // An index column's collation is the one the *index* declared, and the
        // column's own only when the index did not name one. That is SQLite's
        // rule and it is the order the entries are physically in, which is what
        // the tree's comparisons have to agree with.
        let collation = if column.collation.is_empty() {
            table
                .columns
                .get(declared)
                .map(|info| collation_of(&info.collation))
                .unwrap_or(Collation::Binary)
        } else {
            collation_of(&column.collation)
        };
        if collation != Collation::Binary {
            // A tree ordered by anything but BINARY is still *seekable* - the
            // comparisons below use the collation - but it is not "already
            // sorted" for an `ORDER BY` that did not name the same collation,
            // and the executor's streaming rules cannot express that
            // distinction. Disqualifying it costs a sort and never an answer.
            ordered = false;
        }
        if column.descending {
            // And a descending key column for the same reason, now that one
            // means what it says. `SourceLayout::key_columns` is read by rules
            // that ask only *which* columns the walk is ordered by - never in
            // which direction - so a descending tree reported through it says
            // "ascending by a, then b" about a walk that is descending by a.
            // `SELECT a, b FROM t ORDER BY a, b` over `t(a DESC, b)` then
            // skipped its sorter and came back in the index's own order, which
            // is the reverse of the answer.
            //
            // The planner's own `ordering_provided` is direction-aware and
            // still elides the sort where a walk really does answer the
            // ordering, so what this gives up is the executor's second,
            // direction-blind derivation of the same claim.
            ordered = false;
        }
        columns.push(
            ColumnSpec::key(physical)
                .with_collation(collation)
                .with_descending(column.descending),
        );
        types.push(static_type);
        if let Some(slot) = slots.get_mut(declared) {
            *slot = Some(position);
        }
    }
    // What identifies the table row, at the end of the entry.
    let indexed = index.columns.len();
    let identity: Vec<usize> = if trailing.is_empty() {
        // An ordinary table: one rowid column, which is also the table's
        // rowid-alias column when it declared one.
        columns.push(ColumnSpec::key(PhysicalType::Int64));
        types.push(StaticType::Int);
        if let Some(alias) = table.rowid_alias.map(usize::from) {
            if let Some(slot) = slots.get_mut(alias) {
                *slot = Some(indexed);
            }
        }
        vec![indexed]
    } else {
        // A `WITHOUT ROWID` table: its primary key, in its key order. Each of
        // those columns is genuinely carried by this tree, so its slot is
        // mapped and a query reading a primary-key column can be answered from
        // the entry - which is what an index on such a table is worth.
        for (offset, declared) in trailing.iter().enumerate() {
            let (physical, static_type) = match table.columns.get(*declared) {
                Some(info) => physical_for(info.affinity),
                None => (PhysicalType::Any, StaticType::Unknown),
            };
            let collation = table
                .columns
                .get(*declared)
                .map(|info| collation_of(&info.collation))
                .unwrap_or(Collation::Binary);
            if collation != Collation::Binary {
                ordered = false;
            }
            columns.push(ColumnSpec::key(physical).with_collation(collation));
            types.push(static_type);
            if let Some(slot) = slots.get_mut(*declared) {
                // An indexed column that is also a primary-key column keeps the
                // slot it already has: the entry holds it twice, and reading the
                // first copy is what the planner already expects.
                if slot.is_none() {
                    *slot = Some(indexed.saturating_add(offset));
                }
            }
        }
        (indexed..key_columns).collect()
    };
    let rowid_position = trailing.is_empty().then_some(indexed);

    (
        columns,
        SourceLayout {
            tree_key: root,
            slots,
            rowid: rowid_position,
            identity,
            types,
            width: key_columns,
            // An index tree is ordered by every column of its entry, in order:
            // the indexed columns then whatever identifies the row. A
            // descending index column would break that, which is why one
            // disqualifies the tree above.
            key_columns: if ordered {
                (0..key_columns).collect()
            } else {
                Vec::new()
            },
        },
    )
}

/// Returns the declared columns that identify one of a table's rows.
///
/// Empty for a rowid table, whose rows are identified by a rowid rather than by
/// any declared column; the primary key's columns in key order for a `WITHOUT
/// ROWID` one. It is the one place that answers the question, so the index
/// shape, the build path and the write path cannot disagree about what an entry
/// carries.
///
/// @param table - the table
pub fn identity_columns(table: &TableInfo) -> Vec<usize> {
    if !table.without_rowid {
        return Vec::new();
    }
    let mut keyed: Vec<(u16, usize)> = table
        .columns
        .iter()
        .enumerate()
        .filter_map(|(slot, column)| column.primary_key_position.map(|at| (at, slot)))
        .collect();
    keyed.sort_unstable();
    keyed.into_iter().map(|(_, slot)| slot).collect()
}

/// Reports whether an index's tree may stand in for a scan of its table.
///
/// **A partial index holds only the rows its predicate accepted, so it may
/// not.** The physical pass replaces a plain table scan with a scan of the
/// smallest tree that carries every column the query reads, and it decides
/// "carries every column" by building the pipeline against the candidate's
/// layout and seeing whether it translates. That test is about *columns*; it
/// cannot see that a tree holds fewer rows than the table, so a partial index
/// offered as a candidate answers `SELECT rowid FROM t` with the rows inside
/// the predicate and no error.
///
/// It was measured exactly that way: with `CREATE INDEX ix ON u(a) WHERE b > 5`
/// on a two-row table, `SELECT rowid FROM u` returned one row while
/// `SELECT rowid FROM u WHERE b = 1` - which the index cannot cover, so it fell
/// back to the table - returned the other. Both plans said `SCAN u`, because
/// this substitution happens after the planner has spoken.
///
/// An index on an *expression* is fine here: it holds an entry for every row,
/// and the columns it does not carry are unmapped in its layout, so the
/// translation test already refuses it for a query that reads one.
///
/// @param index - the index being considered
fn covers_every_row(index: &IndexInfo) -> bool {
    index.partial_sql.is_none()
}

/// Returns the collation a folded name refers to.
///
/// The three built-in ones. An application-defined collation is not something
/// the new engine can order a tree by - it would have to call back into the
/// connection that registered it on every comparison - so it is treated as
/// BINARY here and the index that uses it is disqualified from being "already
/// sorted", which is the same conservative answer a descending column gets.
///
/// @param folded - the collation's folded name, empty for none
fn collation_of(folded: &[u8]) -> Collation {
    match folded.to_ascii_uppercase().as_slice() {
        b"NOCASE" => Collation::NoCase,
        b"RTRIM" => Collation::RTrim,
        _ => Collation::Binary,
    }
}

/// Chooses a mini-column layout for a declared affinity.
///
/// The mapping is the obvious one and the honesty is in what it does *not*
/// claim: `Blob` affinity (SQLite's "no affinity") gets the `Any` layout, since
/// a column with no affinity has no type to specialise on, and `Numeric` gets
/// `Any` too because it holds integers and reals interchangeably.
///
/// @param affinity - the declared affinity
fn physical_for(affinity: inillucent_value::affinity::Affinity) -> (PhysicalType, StaticType) {
    use inillucent_value::affinity::Affinity;
    match affinity {
        Affinity::Integer => (PhysicalType::Int64, StaticType::Int),
        Affinity::Real => (PhysicalType::Float64, StaticType::Real),
        Affinity::Text => (PhysicalType::Text, StaticType::Text),
        Affinity::Blob => (PhysicalType::Blob, StaticType::Unknown),
        Affinity::Numeric | Affinity::FlexNum => (PhysicalType::Any, StaticType::Unknown),
    }
}

/// Everything one database file's catalog tree describes.
///
/// **One derivation, for `main` and for every `ATTACH`ed file.** The shapes a
/// query is planned against are derived from the `CREATE` text the catalog row
/// carries; deriving them twice - once for the file a connection was opened on
/// and once for a file it attached - is how the two come to disagree about a
/// generated column or a `WITHOUT ROWID` key, which is a wrong answer rather
/// than a refusal.
struct LoadedSchema {
    /// Every tree this file holds, keyed by the connection's handle for it.
    trees: HashMap<u32, PagedTree>,
    /// Each tree's layout, keyed the same way.
    layouts: HashMap<u32, std::rc::Rc<SourceLayout>>,
    /// For each table's handle, its index handles.
    covering: HashMap<u32, Vec<u32>>,
    /// The catalog rows, with the handle each object's tree is registered under.
    entries: Vec<Recorded>,
    /// The tables the binder resolves names against, `sqlite_schema` excepted.
    tables: Vec<TableInfo>,
    /// This file's own `sqlite_schema` declaration.
    schema_info: TableInfo,
    /// The handle each of this file's local tree identifiers is registered under.
    handles: HashMap<u64, u32>,
    /// The objects whose `CREATE` text this engine could not re-read.
    skipped: Vec<String>,
    /// The largest local identifier the file holds, so the next one is past it.
    highest_identifier: u32,
}

/// Reads one file's catalog tree and derives everything needed to plan on it.
///
/// @param database - the file
/// @param catalog_tree - its catalog tree, already attached from the meta page
/// @param index - which schema this is, as the binder numbers them
/// @param name - the schema's name, which the foreign-key planner qualifies with
/// @param catalog_handle - the handle this file's `sqlite_schema` is read through
/// @param allocate - hands out the connection's handle for a local tree identifier
fn load_schema(
    database: &Database,
    catalog_tree: PagedTree,
    index: usize,
    name: &[u8],
    catalog_handle: u32,
    allocate: &mut dyn FnMut(u32) -> u32,
) -> DbResult<LoadedSchema> {
    let mut trees: HashMap<u32, PagedTree> = HashMap::new();
    let mut layouts: HashMap<u32, std::rc::Rc<SourceLayout>> = HashMap::new();
    let mut covering: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut entries: Vec<(i64, SchemaEntry)> = Vec::new();
    let mut identifiers: Vec<u32> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut handles: HashMap<u64, u32> = HashMap::new();
    // **Read with the rowid each row is stored under, not without it.**
    // The two loops below visit the tables and then the indexes, which is
    // not the order the catalog holds them in - a schema that creates a
    // table, an index, another table interleaves the two. The rowid used to
    // be reconstructed from the position in *this* reordered list, so every
    // object after the first index was numbered as some other object. The
    // number is what `seal` and every later `DROP` write by, so the next
    // catalog write landed on the wrong row: rows came back duplicated and
    // rows came back missing.
    let stored_rows = inillucent_catalog::paged::read_catalog_rows(database.pool(), &catalog_tree)?;
    let stored: Vec<SchemaEntry> = stored_rows.iter().map(|(_, entry)| entry.clone()).collect();
    let rowid_of_name = |entry: &SchemaEntry| -> i64 {
        stored_rows
            .iter()
            .find(|(_, held)| held.name == entry.name && held.kind == entry.kind)
            .map(|(rowid, _)| *rowid)
            .unwrap_or_default()
    };

    // **The identifier comes out of the catalog row, not out of a counter.**
    // It used to be handed out here in catalog order, on the reasoning that
    // it was this process's own bookkeeping. It is not: every logical row
    // record in the log carries it, so a reader that numbered trees
    // differently from the writer would hand recovery's records to the wrong
    // tree - a wrong answer rather than a refusal. The identifier is stored in
    // the catalog now; this reads it back.
    //
    // `next_root` is set past the largest so a `CREATE TABLE` after this
    // open cannot collide with one already in the file, which a counter that
    // restarted at every open could and did.
    let mut highest_identifier = 0u32;
    // Every table by folded name, because an index's shape is derived
    // against its table's declaration and the catalog does not order tables
    // before their indexes.
    let mut infos: HashMap<Vec<u8>, (u32, TableInfo)> = HashMap::new();

    for entry in &stored {
        if entry.kind != ObjectKind::Table {
            continue;
        }
        // A virtual table has **no tree of its own**. Its rows live in the
        // shadow tables the module declared, which are ordinary tables in
        // this same catalog and are loaded by this same loop. So its row
        // carries no tree identifier, and asking for one refused to open
        // every database holding a search table - which is how this was
        // found, by moving `inillucent-migrate` onto the engine.
        //
        // The kind is learned from a throwaway parse rather than from the
        // parse below, because that one is given the identifier and every
        // shape it derives is derived against it. Parsing once with a
        // placeholder root and patching `info.root` afterwards looked like
        // the same thing and was not: it left the *derived* shapes pointing
        // at the placeholder, and every table then scanned the same tree -
        // `count(*)` answered the same number for every table in the file.
        if matches!(
            table_from_create_sql(&entry.sql, index, 0).map(|info| info.kind),
            Ok(inillucent_sql::catalog_view::TableKind::Virtual)
        ) {
            // `entries` and `identifiers` are zipped into `Recorded` below,
            // so they are parallel and a row pushed to one has to be pushed
            // to the other. Pushing only the entry shifted every later
            // object onto the previous one's tree - which read as a table
            // whose covering index answered another table's rows, and cost
            // an afternoon to find. Zero is what `Recorded.root` documents
            // for an object with no tree.
            entries.push((rowid_of_name(entry), entry.clone()));
            identifiers.push(0);
            continue;
        }
        let local = identifier_of(entry)?;
        highest_identifier = highest_identifier.max(local);
        let identifier = allocate(local);
        handles.insert(u64::from(local), identifier);
        let mut info = match table_from_create_sql(&entry.sql, index, identifier) {
            Ok(info) => info,
            Err(_) => {
                skipped.push(String::from_utf8_lossy(&entry.name).into_owned());
                continue;
            }
        };
        info.root = identifier;
        let (columns, key_columns, layout) = if info.without_rowid {
            match keyed_table_shape(&info) {
                Ok((columns, key_columns, layout)) => (columns, key_columns, layout),
                Err(_) => {
                    skipped.push(String::from_utf8_lossy(&entry.name).into_owned());
                    continue;
                }
            }
        } else {
            let (columns, layout) = table_shape(&info);
            (columns, 1, layout)
        };
        let tree = PagedTree::attach(
            database.pool(),
            // **The tree keeps the identifier its own file numbered it with.**
            // Every log record it writes carries this number and the log
            // outlives the process, so it is the file's business. The map key
            // beside it is the connection's handle, which is not.
            u64::from(local),
            entry.root,
            columns,
            key_columns,
            entry.stats.leaf_count,
            entry.stats.row_count,
        )?;
        trees.insert(identifier, tree);
        layouts.insert(identifier, std::rc::Rc::new(layout));
        infos.insert(info.folded.clone(), (identifier, info.clone()));
        entries.push((rowid_of_name(entry), entry.clone()));
        identifiers.push(identifier);
    }

    for entry in &stored {
        if entry.kind != ObjectKind::Index {
            continue;
        }
        let folded = entry.table.to_ascii_lowercase();
        let Some((table_root, table_info)) = infos.get(&folded).cloned() else {
            skipped.push(String::from_utf8_lossy(&entry.name).into_owned());
            continue;
        };
        let local = identifier_of(entry)?;
        highest_identifier = highest_identifier.max(local);
        let identifier = allocate(local);
        handles.insert(u64::from(local), identifier);
        // **An automatic index is declared by the *table's* text**, and the
        // catalog stores an empty statement for it - which is what SQLite
        // writes for `sqlite_autoindex_t_1`. Parsing that empty text as a
        // `CREATE INDEX` fails, and the arm below used to skip the index: no
        // tree was attached, no row joined `entries`, and the declaration the
        // planner reads kept the root of zero it was parsed with. The visible
        // result was that **any query using a non-`INTEGER PRIMARY KEY` failed
        // after a reopen** - `EXPLAIN QUERY PLAN` named the index and the
        // statement answered `no layout imported for root page 0`. It is the
        // same rule `tables_from_entries` and `shape_of` already apply.
        let index = if entry.sql.is_empty() {
            let wanted = entry.name.to_ascii_lowercase();
            match table_info
                .indexes
                .iter()
                .find(|index| index.folded == wanted)
            {
                Some(index) => {
                    let mut index = index.clone();
                    index.root = identifier;
                    index
                }
                None => {
                    skipped.push(String::from_utf8_lossy(&entry.name).into_owned());
                    continue;
                }
            }
        } else {
            match inillucent_catalog::load::index_from_create_sql(
                &entry.sql,
                &table_info,
                identifier,
            ) {
                Ok(index) => index,
                Err(_) => {
                    skipped.push(String::from_utf8_lossy(&entry.name).into_owned());
                    continue;
                }
            }
        };
        let (columns, layout) = index_shape(&table_info, &index, identifier);
        let key_columns = columns.len();
        let tree = PagedTree::attach(
            database.pool(),
            // **The tree keeps the identifier its own file numbered it with.**
            // Every log record it writes carries this number and the log
            // outlives the process, so it is the file's business. The map key
            // beside it is the connection's handle, which is not.
            u64::from(local),
            entry.root,
            columns,
            key_columns,
            entry.stats.leaf_count,
            entry.stats.row_count,
        )?;
        trees.insert(identifier, tree);
        layouts.insert(identifier, std::rc::Rc::new(layout));
        if covers_every_row(&index) {
            covering.entry(table_root).or_default().push(identifier);
        }
        // The index joins its table's declaration, so the binder offers it
        // to the planner exactly as the import does. An automatic one is
        // already there - the table's own text declared it - so its root is
        // filled in rather than a second copy pushed.
        if let Some((_, info)) = infos.get_mut(&folded) {
            match info
                .indexes
                .iter_mut()
                .find(|held| held.folded == index.folded)
            {
                Some(held) => held.root = identifier,
                None => info.indexes.push(index),
            }
        }
        entries.push((rowid_of_name(entry), entry.clone()));
        identifiers.push(identifier);
    }

    // **The triggers, then the keys, and in that order.** A written
    // trigger is a catalog row like a table or an index and joins its
    // table's declaration; a foreign key is a trigger the binder writes,
    // and `plan_schema` can only write it once every table is in hand,
    // because a key records only the child's side and the parent's has to
    // be found by asking every table what it points at.
    //
    // Neither was done here until now, which is the whole reason foreign
    // keys were unenforced: the binder fills a statement's `triggers` from
    // exactly these two places, and both were empty on this engine.
    // **A view is a row and nothing else, and this loop was missing.** The
    // catalog carried it - `SELECT type, name FROM sqlite_schema` listed
    // `view|v` - but nothing put it into `entries`, so `tables_from_entries`
    // never saw it and the binder never learned the name. `SELECT * FROM v`
    // answered `no such table: v` against a schema that says the view is
    // there, which is worse than a schema that dropped it: the object is
    // listed and unreadable.
    //
    // It was not only the migration path. A view created by `CREATE VIEW`,
    // queried, and then read again after a close and reopen was gone the same
    // way, because this is the function every open goes through.
    //
    // Before the triggers, so an `INSTEAD OF` trigger finds the view it is
    // attached to.
    for entry in &stored {
        if entry.kind != ObjectKind::View {
            continue;
        }
        entries.push((rowid_of_name(entry), entry.clone()));
        identifiers.push(0);
    }
    for entry in &stored {
        if entry.kind != ObjectKind::Trigger {
            continue;
        }
        let folded = entry.table.to_ascii_lowercase();
        let Some((_, info)) = infos.get_mut(&folded) else {
            skipped.push(String::from_utf8_lossy(&entry.name).into_owned());
            continue;
        };
        match inillucent_catalog::load::trigger_from_create_sql(&entry.sql) {
            // Newest first, which is SQLite's own order: it pushes each
            // trigger onto the front of the table's list as it reads the
            // schema, so the most recently created one fires first.
            Ok(trigger) => info.triggers.insert(0, trigger),
            Err(_) => skipped.push(String::from_utf8_lossy(&entry.name).into_owned()),
        }
        entries.push((rowid_of_name(entry), entry.clone()));
        identifiers.push(0);
    }

    let mut planned: Vec<TableInfo> = infos.values().map(|(_, info)| info.clone()).collect();
    inillucent_sql::foreign_key::plan_schema(&mut planned, name, &Limits::default());
    for info in planned {
        if let Some((_, held)) = infos.get_mut(&info.folded) {
            held.foreign_key_triggers = info.foreign_key_triggers.clone();
        }
    }

    // `sqlite_schema` over the catalog tree, exactly as the import builds
    // it: one root number no object can have, and the ordinary scan path.
    let schema_root = catalog_handle;
    let schema_info = table_from_create_sql(schema_create_sql(), index, schema_root)?;
    layouts.insert(
        schema_root,
        std::rc::Rc::new(SourceLayout {
            tree_key: schema_root,
            slots: (1..=5).map(Some).collect(),
            rowid: Some(0),
            identity: vec![0],
            types: vec![
                StaticType::Int,
                StaticType::Text,
                StaticType::Text,
                StaticType::Text,
                StaticType::Int,
                StaticType::Text,
            ],
            width: 6,
            key_columns: vec![0],
        }),
    );
    trees.insert(schema_root, catalog_tree);
    for roots in covering.values_mut() {
        roots.sort_by_key(|root| {
            trees
                .get(root)
                .map(PagedTree::byte_size)
                .unwrap_or(usize::MAX)
        });
    }
    let mut tables: Vec<TableInfo> = infos.into_values().map(|(_, info)| info).collect();
    tables.sort_by(|one, two| one.folded.cmp(&two.folded));
    attach_statistics(database.pool(), &trees, &mut tables);
    Ok(LoadedSchema {
        trees,
        layouts,
        covering,
        entries: entries
            .into_iter()
            .zip(identifiers)
            .map(|((rowid, entry), root)| Recorded { rowid, root, entry })
            .collect(),
        tables,
        schema_info,
        handles,
        skipped,
        highest_identifier,
    })
}

/// Reads `sqlite_stat1` onto the tables it describes.
///
/// **The half of `ANALYZE` that was missing.** This engine wrote the table and
/// never read it: `IndexInfo::prefix_rows` and `TableInfo::analysed_rows` are
/// what the planner costs a join with, and nothing on this path had ever set
/// them - so a file the reference had `ANALYZE`d arrived here with its
/// measurements sitting in a table nobody opened, and the join was planned by
/// the guesses the measurements exist to replace. `inillucent-catalog`'s own
/// loader does this for a SQLite file; this is the same rule over a PAX tree.
///
/// A missing, empty or unreadable statistics table is not an error - statistics
/// are a hint, and a planner that refused to run without them would turn
/// `ANALYZE` into a dependency.
///
/// @param pool - the buffer pool the trees live in
/// @param trees - every tree this schema holds, by handle
/// @param tables - the binder's tables, patched in place
fn attach_statistics(pool: &Pool, trees: &HashMap<u32, PagedTree>, tables: &mut [TableInfo]) {
    let folded = inillucent_catalog::analyze::STAT1
        .as_bytes()
        .to_ascii_lowercase();
    let Some(root) = tables
        .iter()
        .find(|held| held.folded == folded)
        .map(|held| held.root)
    else {
        return;
    };
    let Some(tree) = trees.get(&root) else {
        return;
    };
    let rows = statistics_rows(pool, tree);
    apply_statistics(tables, &rows);
}

/// Reads the three-column rows out of a `sqlite_stat1` tree.
///
/// Separate from attaching them so a caller holding `&mut self` can read under
/// an immutable borrow, drop it, and then patch its tables.
///
/// @param pool - the buffer pool the tree lives in
/// @param tree - the statistics tree
fn statistics_rows(pool: &Pool, tree: &PagedTree) -> Vec<(Vec<u8>, Option<Vec<u8>>, Vec<u8>)> {
    let mut rows: Vec<(Vec<u8>, Option<Vec<u8>>, Vec<u8>)> = Vec::new();
    let _ = tree.visit_leaves(pool, &mut |leaf| {
        for row in leaf.live()? {
            // The row is the rowid and then the three columns SQLite's own
            // `sqlite_stat1` carries: the table, the index, the measurement.
            let Some(Datum::Text(table)) = row.get(1) else {
                continue;
            };
            let index = match row.get(2) {
                Some(Datum::Text(name)) => Some(name.to_vec()),
                _ => None,
            };
            let Some(Datum::Text(stat)) = row.get(3) else {
                continue;
            };
            rows.push((table.to_vec(), index, stat.to_vec()));
        }
        Ok(true)
    });
    rows
}

/// Clears every table's measurements and applies the ones just read.
///
/// @param tables - the binder's tables, patched in place
/// @param rows - the `sqlite_stat1` rows
fn apply_statistics(tables: &mut [TableInfo], rows: &[(Vec<u8>, Option<Vec<u8>>, Vec<u8>)]) {
    // A stale reading is worse than none, so what is there now replaces
    // whatever a previous load left behind rather than adding to it.
    for table in tables.iter_mut() {
        table.analysed_rows = None;
        for index in &mut table.indexes {
            index.prefix_rows = Vec::new();
        }
    }
    for (table, index, stat) in rows {
        inillucent_catalog::load::apply_statistic(tables, table, index.as_deref(), stat);
    }
}

/// One database file, opened, recovered, and ready to be read.
struct OpenedFile {
    /// The pool, the meta page and the free map.
    database: Database,
    /// The log, positioned where recovery ended.
    wal: std::rc::Rc<Wal>,
    /// The catalog tree, attached from the meta page's root.
    catalog_tree: PagedTree,
    /// The highest transaction number any record recovery scanned carried.
    ///
    /// **A reopened database must not reuse a number the log still holds**, and
    /// this is what the engine's counter is started above. Recovery decides
    /// which records to replay by transaction number, so a number used twice in
    /// one log makes two different transactions into one - a run that wrote as
    /// transaction 3 and crashed leaves records the *next* run resurrects the
    /// moment its own transaction 3 commits, permanently.
    ///
    /// `inillucent-wal` has reported this since it was written and nothing read
    /// it; the cross-file commit is what made it load-bearing, because a marker
    /// naming transaction 7 in a file whose next run also calls something
    /// transaction 7 would suppress a commit that had nothing to do with it.
    highest_txn: u64,
}

/// Returns where the log resumes, raising it above every stamp the file carries.
///
/// **A page's LSN has to be a position in the stream currently beside the file,
/// and after a recovery whose chain was short of what the pages reflect it is
/// not.** Recovery applies a record to a page only when the page's
/// stamp is below the record's, so a page stamped by a stream that no longer
/// exists silently swallows every later write to it - the record is skipped,
/// the file stays structurally intact, and nothing anywhere says a committed row
/// was lost. It is how Nikaya's mail database ended up with page 3 stamped
/// 21,939,058,496 beside a log ending at 21,075,008,440, after 24 segments were
/// moved aside to recover it.
///
/// So the log resumes at `max(recovered.next_lsn, high_water + 1)`. In every
/// healthy file the first term already wins and this changes nothing: the
/// write-ahead rule puts every stamp below the log's durable end, and the
/// durable end is at or below where recovery stopped. It fires only on a file
/// whose log is short of what its pages carry.
///
/// Two things follow from an LSN being a **byte offset inside a segment**:
///
/// 1. The jump takes the *next* sequence. The write offset of a record is
///    `header + (lsn - segment.first_lsn)`, so resuming 864 million positions
///    into the segment recovery stopped in would ask for an 864 MB file.
/// 2. The meta page is checkpointed before a record is written at the new
///    position. That leaves a gap between the old segment's last byte and the
///    new one's first, and `read_chain` stops a chain at a gap - correctly,
///    since a gap is otherwise a lost segment - so the next recovery has to
///    start *inside* the new segment rather than walk up to it. The claim the
///    checkpoint makes is true at that moment: the replay's pages have just been
///    flushed, and there are no records between the chain's end and the new
///    position.
///
/// @param database - the recovered file, whose pool carries the high water
/// @param outcome - what recovery found
fn resume_above_every_stamp(
    database: &mut Database,
    outcome: &inillucent_wal::Recovered,
) -> DbResult<(u64, u64)> {
    let next_lsn = outcome.next_lsn.max(FIRST_LSN);
    let sequence = outcome.sequence.max(1);
    // Read off the pool rather than off the meta record. `Database::open` seeds
    // it with what the meta page carried and `Pool::writeback` has raised it for
    // every page this recovery has already evicted, so it is the higher of the
    // two and never the lower.
    let high_water = database.pool().high_water_lsn();
    if high_water < next_lsn {
        return Ok((next_lsn, sequence));
    }
    let resumed = high_water.saturating_add(1);
    let rolled = sequence.saturating_add(1);
    database.set_log_position(resumed, outcome.latest_cts, rolled);
    database.checkpoint()?;
    Ok((resumed, rolled))
}

/// Opens one database file, replays its log into it, and opens that log.
///
/// **The one recovery path, for the file a connection is opened on and for
/// every file it attaches.** An `ATTACH`ed database is an ordinary database of
/// this engine - it may have been written by a process that crashed, and a
/// second recovery path would be a second set of rules about what a torn tail
/// means. There is one, and both callers take it.
///
/// @param vfs - the file system the file and its log live on
/// @param db_path - the database file
/// @param frames - how many frames the buffer pool holds
/// @param doubtful - transactions whose `Commit` record is not the decision
fn open_file(
    vfs: &std::sync::Arc<dyn inillucent_vfs::Vfs>,
    db_path: &DbPath,
    frames: usize,
    doubtful: &std::collections::BTreeSet<u64>,
) -> DbResult<OpenedFile> {
    let database = Database::open(vfs.as_ref(), db_path, frames.max(64))?;

    // **Recovery.** The log is replayed into the file before anything is read
    // out of it, which is what makes this an open rather than a reader of
    // whatever the last checkpoint happened to leave behind.
    //
    // It could not be done before a tree's identifier was stored in the
    // catalog. `TreeRows` is keyed by that identifier, every logical row record
    // carries it, and until then the writer's numbering and a reader's were
    // different - so a replay would have put rows into the wrong tree, which is
    // a wrong answer rather than a refusal.
    //
    // From the file's own checkpoint, not from the start of the log:
    // `RecoveryStart::fresh` scans from `FIRST_LSN` and would replay everything
    // the last checkpoint already applied.
    //
    // `doubtful` is how a cross-file commit reaches this. A transaction that
    // wrote two databases votes in each file's log and is *decided* by a
    // super-journal outside both, so a `Commit` record for one of those
    // transactions is a vote rather than the decision - see
    // `super_journal_doubt`.
    let meta = database.meta();
    let start = if meta.checkpoint_lsn == 0 {
        inillucent_wal::RecoveryStart {
            doubtful: doubtful.clone(),
            ..inillucent_wal::RecoveryStart::fresh(database.uuid())
        }
    } else {
        inillucent_wal::RecoveryStart {
            uuid: database.uuid(),
            checkpoint_lsn: meta.checkpoint_lsn,
            sequence: meta.wal_sequence,
            cts_watermark: meta.cts_watermark,
            doubtful: doubtful.clone(),
        }
    };
    let mut database = database;
    // The shapes come from the catalog as it stood at the last checkpoint, plus
    // the catalog tree itself, whose own rows are what a `CREATE TABLE` writes.
    // A record naming a tree that is in none of them - a table created *after*
    // the checkpoint, whose rows were then written - makes `TreeRows` refuse,
    // which fails this open with a named error rather than replaying into a
    // tree that is not the one meant.
    let checkpointed = {
        let before = attach_catalog(database.pool(), database.catalog_root())?;
        read_catalog(database.pool(), &before)?
    };
    let (outcome, free_map) = {
        let mut applier =
            inillucent_txn::redo::Applier::new(&mut database, LearningRows::new(&checkpointed));
        let outcome = inillucent_wal::recover(vfs.as_ref(), db_path, start, &mut applier)?;
        (outcome, applier.free_map_changes().to_vec())
    };
    // The free map is rebuilt after the scan rather than inside it: the map and
    // every page write are both behind `&mut Database`, and one record cannot
    // hold two mutable borrows of the same object.
    //
    // **In log order.** Claiming every allocation and then
    // releasing every free gave the frees the last word, so a page freed and
    // allocated again inside the replayed range came back free while it was
    // live, and the next allocation handed it to a second owner. See
    // `Applier::free_map_changes`.
    for change in &free_map {
        match change.allocated {
            true => database.claim(change.page)?,
            false => database.release(change.page, 1)?,
        }
    }
    inillucent_wal::truncate_after(vfs.as_ref(), db_path, &outcome)?;

    // **The log resumes where recovery ended, not at the beginning.** Opening it
    // at `FIRST_LSN` with sequence 1 starts a second stream over the same
    // segments: the session writes records the *next* open cannot find, because
    // the meta page's checkpoint points into the first stream. A test caught it
    // as a table created after an open vanishing on the one after that -
    // `no such table: second` from a file that had just been told to make it.
    //
    // **And above every stamp the file carries.** See
    // `resume_above_every_stamp`.
    let (next_lsn, sequence) = resume_above_every_stamp(&mut database, &outcome)?;
    let wal = std::rc::Rc::new(Wal::open(
        std::sync::Arc::clone(vfs),
        db_path,
        database.uuid(),
        next_lsn,
        sequence,
        WalOptions::default(),
    )?);
    database.pool().set_durable_lsn(wal.write_ahead_point());
    let_the_pool_ask_the_log(database.pool(), &wal);

    // The catalog is read again, because recovery may have changed it: a
    // `CREATE TABLE` after the checkpoint is a row in this very tree.
    let catalog_tree = attach_catalog(database.pool(), database.catalog_root())?;
    Ok(OpenedFile {
        database,
        wal,
        catalog_tree,
        highest_txn: outcome.highest_txn,
    })
}
