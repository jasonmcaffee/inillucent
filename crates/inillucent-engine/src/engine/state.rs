//! `ImportedDatabase`'s state, in the six groups it is made of.
//!
//! Invariant: **a method here reads one group and calls nothing on the
//! database.** That is the rule that put it here, and it was counted rather
//! than judged: per method, which groups it names and whether it calls another
//! method of the type. A method that grows a second group's field belongs back
//! in `lib.rs` until A1 step 3 decides where it really lives.
//!
//! ## What the groups are for
//!
//! `ImportedDatabase` had sixty-three fields and 284 methods, so a reader
//! looking for the two fields a change is about had to read all of them. The
//! groups are not a tidy-up: they are the shape A1 step 3 splits along.
//! [`Storage`] becomes `Database`, [`SessionState`] becomes `Session`,
//! [`Writing`] becomes `Writer`, and the single `RefCell` that makes a
//! reentrant call an error (see `crate::connect`) becomes three.
//!
//! What is left on `ImportedDatabase` is the methods that genuinely span two
//! groups, and the ones a caller outside this crate reaches - the profiling
//! binaries and the executor's catalog view among them - which stay where a
//! caller can find them and read their group through the field.

use crate::*;

/// One connection's settings: everything a `PRAGMA` or an `sqlite3_limit` call
/// sets and everything else only reads.
///
/// **Its own group, behind its own cells (task-1962, A1 step 3).** TDD section
/// 11 names `pragmas` as a member of `Session`, and this is that member. Every
/// field is behind a cell, so the group is reachable through a shared reference
/// and [`crate::connect::Database`] holds a second handle on the same one. That
/// is what lets `sqlite3_limit` and `PRAGMA defensive` be answered while a
/// statement is running, which is where an application asks them from.
///
/// The cells are not about threads. `Pragmas` is `Rc`, not `Arc`, and a
/// connection is still single-threaded; the cell is what makes a *shared*
/// reference enough to write through.
pub(crate) struct Pragmas {
    /// The bounds a statement is parsed and planned under.
    limits: std::cell::RefCell<Limits>,

    /// Which planner optimizations are on.
    ///
    /// Per connection rather than per statement, because a lever is a question
    /// about the *planner* - "is the answer the same with this off" - and a
    /// measurement that varied it per statement would be comparing two plans of
    /// two different queries.
    levers: std::cell::Cell<Levers>,

    /// How long a writer waits for the file, in milliseconds.
    ///
    /// **It governs the cross-process wait rather than only being remembered
    /// (task-1979, C7).** It used to be a value this connection recorded and
    /// reported while the wait that actually happened was a constant in
    /// `inillucent-pool` no pragma, flag or environment variable could reach.
    /// Setting it now pushes the number into `Database::set_busy_millis`, which
    /// is the budget `begin_write_within` spends, and it starts at that
    /// crate's `DEFAULT_BUSY_MILLIS` rather than at zero so the behaviour a
    /// caller gets without asking is the one they always got.
    busy_timeout_ms: std::cell::Cell<u64>,

    /// Whether `PRAGMA foreign_keys` is on.
    foreign_keys: std::cell::Cell<bool>,

    /// Whether `PRAGMA defer_foreign_keys` has put every immediate check off
    /// until the commit, for the transaction now open.
    defer_foreign_keys: std::cell::Cell<bool>,

    /// Whether the file lock is held between transactions.
    ///
    /// **`normal` is a real setting now, and the reason it can be reported
    /// honestly.** Before that, this engine took no file lock at all and
    /// reported `exclusive`, which was the closest true description of "nobody
    /// else may touch this". Under `normal` the lock is taken for each
    /// transaction and released after it, so a second process may have the file
    /// in between - which is what the word means. `exclusive` keeps it, which
    /// is faster and is what a single-process application wants.
    locking_exclusive: std::cell::Cell<bool>,

    /// How the pre-commit state is protected, which `PRAGMA journal_mode` sets.
    ///
    /// The write-ahead log by default, because it is the faster of the two -
    /// one sync per commit against two. A rollback journal is what an
    /// application selects when it wants the database to be one file after a
    /// clean close, which is the reason a rollback journal is supported at all.
    journal_mode: std::cell::Cell<inillucent_pool::journal::JournalMode>,

    /// Whether `PRAGMA ignore_check_constraints` has turned `CHECK` off.
    ///
    /// Like `foreign_keys` it is read by the *binder*, so changing it throws
    /// away the compiled statements: a plan built while checks were on carries
    /// them and would keep carrying them after the pragma turned them off.
    ignore_check_constraints: std::cell::Cell<bool>,

    /// What `PRAGMA secure_delete` is set to: 0 off, 1 on, 2 fast.
    ///
    /// On, the bytes a deleted row occupied are overwritten before the space is
    /// reused, so a row that has been deleted is not still readable in the file
    /// by anyone who opens it with a hex editor. Off is SQLite default and
    /// this engine default, because the overwrite is a write.
    secure_delete: std::cell::Cell<u8>,

    /// What `PRAGMA auto_vacuum` is set to: 0 none, 1 full, 2 incremental.
    ///
    /// Settable only while the database holds no table, which is SQLite rule -
    /// the mode decides how the file is laid out, and changing it afterwards is
    /// what `VACUUM` is for.
    auto_vacuum: std::cell::Cell<u8>,

    /// Whether `PRAGMA automatic_index` lets the planner build one.
    ///
    /// On by default, as in SQLite: an unindexed table on the inner side of a
    /// join is scanned once per outer row, and building a transient index over
    /// it first is cheaper as soon as the outer side has more than a handful of
    /// rows.
    automatic_index: std::cell::Cell<bool>,

    /// What `PRAGMA cache_size` reads back, in SQLite's own signed units.
    ///
    /// `None` until a caller sets one, when it is the pool's own size in
    /// kibibytes; afterwards it is the caller's number, so reading it always
    /// describes the cache the engine is actually keeping.
    cache_size: std::cell::Cell<Option<i64>>,

    /// Whether `LIKE` compares ASCII letters exactly.
    ///
    /// `PRAGMA case_sensitive_like`. Read by the binder's translation through
    /// `TreeCatalog::like_is_case_sensitive`, and the statement cache is
    /// emptied when it changes so a compiled `LIKE` is never run under the
    /// other setting.
    case_sensitive_like: std::cell::Cell<bool>,

    /// What `PRAGMA analysis_limit` was set to, in rows.
    ///
    /// Recorded and exceeded: `ANALYZE` walks the whole table, which is more
    /// than any cap asks for.
    analysis_limit: std::cell::Cell<i64>,

    /// What `PRAGMA writable_schema` was set to.
    ///
    /// Recorded and reported. There is nothing for it to unlock: the binder
    /// refuses a write to a reserved-prefix table whatever it says, and a
    /// module's shadow table is an ordinary table a write reaches without it.
    writable_schema: std::cell::Cell<bool>,

    /// Whether `SQLITE_DBCONFIG_DEFENSIVE` is in force.
    ///
    /// Off here and on in the shell, which is where SQLite draws the same line:
    /// the library defaults it off and its command-line tool turns it on. What
    /// it forbids is the two statements that can lose a database in one line -
    /// `PRAGMA journal_mode = OFF`, which stops protecting anything, and
    /// `PRAGMA writable_schema = ON`, which lets a caller write a schema row
    /// the engine will later try to parse.
    defensive: std::cell::Cell<bool>,

    /// Whether this connection refuses to write, set by `PRAGMA query_only`.
    ///
    /// Honoured rather than remembered: a caller sets it to make a mistake
    /// impossible, and one that recorded it and wrote anyway would be worse
    /// than an engine that refused the pragma outright.
    query_only: std::cell::Cell<bool>,

    /// Whether a trigger's own writes fire triggers, set by
    /// `PRAGMA recursive_triggers`.
    recursive_triggers: std::cell::Cell<bool>,

    /// The ceiling `PRAGMA max_page_count` set, in pages.
    max_page_count: std::cell::Cell<i64>,

    /// What `PRAGMA temp_store` reports.
    ///
    /// The *setting* rather than the state, which is what SQLite reports: this
    /// engine keeps temporary tables in memory whatever the number says, and
    /// the one value it cannot be - `FILE` - is refused rather than recorded.
    temp_store: std::cell::Cell<i64>,
}

// The accessors, and why the fields below them are private.
//
// **Twenty `pub(crate)` fields written from seventeen other files
// (task-1969, 7.3).** The module comment above says this split is the midpoint
// and that A1 step 3 takes it further; this is that step for the two groups
// whose callers are not the binder's neighbours. `Schema`, `Storage` and
// `SessionState` follow in the `bind.rs` ticket, because their callers and that
// split touch the same lines.
//
// What a private field buys here is not encapsulation of the value - every one
// of these is a `Cell` and a caller could always read and write it - but a
// single place where the read and the write are named. `set_journal_mode` is a
// method a reader can find every caller of; `pragmas.journal_mode.set(...)` is
// a pattern they have to grep for and hope they spelled the same way. It is
// also what makes `set_automatic_index` below possible without being an
// inconsistency: that one does more than assign, and now it sits beside the
// nineteen that do not.
//
// task-1962 declined the full `RefCell` conversion with a reason that still
// holds - `engine/compiled.rs` passes two disjoint `&mut` fields into a
// `WriteView`, a proof the borrow checker does at compile time and a `RefCell`
// would move to run time - and that reason says nothing about accessors.

impl Pragmas {
    /// Returns the cell holding limits.
    ///
    /// The cell rather than a borrow of it, because a caller that hands this to
    /// another type needs the cell itself.
    pub(crate) fn limits(&self) -> &std::cell::RefCell<Limits> {
        &self.limits
    }

    /// Returns analysis limit.
    pub(crate) fn analysis_limit(&self) -> i64 {
        self.analysis_limit.get()
    }

    /// Sets analysis limit.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_analysis_limit(&self, value: i64) {
        self.analysis_limit.set(value);
    }

    /// Returns auto vacuum.
    pub(crate) fn auto_vacuum(&self) -> u8 {
        self.auto_vacuum.get()
    }

    /// Sets auto vacuum.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_auto_vacuum(&self, value: u8) {
        self.auto_vacuum.set(value);
    }

    /// Returns automatic index.
    pub(crate) fn automatic_index(&self) -> bool {
        self.automatic_index.get()
    }

    /// Returns busy timeout ms.
    pub(crate) fn busy_timeout_ms(&self) -> u64 {
        self.busy_timeout_ms.get()
    }

    /// Sets busy timeout ms.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_busy_timeout_ms(&self, value: u64) {
        self.busy_timeout_ms.set(value);
    }

    /// Returns cache size.
    pub(crate) fn cache_size(&self) -> Option<i64> {
        self.cache_size.get()
    }

    /// Sets cache size.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_cache_size(&self, value: Option<i64>) {
        self.cache_size.set(value);
    }

    /// Returns case sensitive like.
    pub(crate) fn case_sensitive_like(&self) -> bool {
        self.case_sensitive_like.get()
    }

    /// Sets case sensitive like.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_case_sensitive_like(&self, value: bool) {
        self.case_sensitive_like.set(value);
    }

    /// Returns defensive.
    pub(crate) fn defensive(&self) -> bool {
        self.defensive.get()
    }

    /// Sets defensive.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_defensive(&self, value: bool) {
        self.defensive.set(value);
    }

    /// Returns defer foreign keys.
    pub(crate) fn defer_foreign_keys(&self) -> bool {
        self.defer_foreign_keys.get()
    }

    /// Sets defer foreign keys.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_defer_foreign_keys(&self, value: bool) {
        self.defer_foreign_keys.set(value);
    }

    /// Returns foreign keys.
    pub(crate) fn foreign_keys(&self) -> bool {
        self.foreign_keys.get()
    }

    /// Sets foreign keys.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_foreign_keys(&self, value: bool) {
        self.foreign_keys.set(value);
    }

    /// Returns ignore check constraints.
    pub(crate) fn ignore_check_constraints(&self) -> bool {
        self.ignore_check_constraints.get()
    }

    /// Sets ignore check constraints.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_ignore_check_constraints(&self, value: bool) {
        self.ignore_check_constraints.set(value);
    }

    /// Returns journal mode.
    pub(crate) fn journal_mode(&self) -> inillucent_pool::journal::JournalMode {
        self.journal_mode.get()
    }

    /// Sets journal mode.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_journal_mode(&self, value: inillucent_pool::journal::JournalMode) {
        self.journal_mode.set(value);
    }

    /// Returns levers.
    pub(crate) fn levers(&self) -> Levers {
        self.levers.get()
    }

    /// Sets levers.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_levers(&self, value: Levers) {
        self.levers.set(value);
    }

    /// Returns locking exclusive.
    pub(crate) fn locking_exclusive(&self) -> bool {
        self.locking_exclusive.get()
    }

    /// Sets locking exclusive.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_locking_exclusive(&self, value: bool) {
        self.locking_exclusive.set(value);
    }

    /// Returns max page count.
    pub(crate) fn max_page_count(&self) -> i64 {
        self.max_page_count.get()
    }

    /// Sets max page count.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_max_page_count(&self, value: i64) {
        self.max_page_count.set(value);
    }

    /// Returns query only.
    pub(crate) fn query_only(&self) -> bool {
        self.query_only.get()
    }

    /// Sets query only.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_query_only(&self, value: bool) {
        self.query_only.set(value);
    }

    /// Returns recursive triggers.
    pub(crate) fn recursive_triggers(&self) -> bool {
        self.recursive_triggers.get()
    }

    /// Sets recursive triggers.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_recursive_triggers(&self, value: bool) {
        self.recursive_triggers.set(value);
    }

    /// Returns secure delete.
    pub(crate) fn secure_delete(&self) -> u8 {
        self.secure_delete.get()
    }

    /// Sets secure delete.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_secure_delete(&self, value: u8) {
        self.secure_delete.set(value);
    }

    /// Returns temp store.
    pub(crate) fn temp_store(&self) -> i64 {
        self.temp_store.get()
    }

    /// Sets temp store.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_temp_store(&self, value: i64) {
        self.temp_store.set(value);
    }

    /// Returns writable schema.
    pub(crate) fn writable_schema(&self) -> bool {
        self.writable_schema.get()
    }

    /// Sets writable schema.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_writable_schema(&self, value: bool) {
        self.writable_schema.set(value);
    }
}

impl Pragmas {
    /// Turns the automatic index on or off, which `PRAGMA automatic_index` does.
    ///
    /// It is its own method rather than a call to `disable_levers` because that
    /// one only ever turns levers *off* - it is the measurement harness's entry
    /// point, and an A/B arm never turns one back on. A pragma has to do both.
    ///
    /// @param on - whether the planner may build one
    pub(crate) fn set_automatic_index(&self, on: bool) {
        let mask = self.levers.get().disabled();
        self.levers.set(Levers::without(if on {
            mask & !Levers::AUTOMATIC_INDEX
        } else {
            mask | Levers::AUTOMATIC_INDEX
        }));
    }

    /// Returns the settings a connection starts with.
    ///
    /// The values are SQLite's own defaults for a fresh connection, and both
    /// open paths - creating a database and opening one - start here, because a
    /// setting is a property of the connection rather than of the file.
    pub(crate) fn fresh() -> Pragmas {
        Pragmas {
            limits: std::cell::RefCell::new(Limits::default()),
            levers: std::cell::Cell::new(Levers::default()),
            busy_timeout_ms: std::cell::Cell::new(inillucent_pool::file::DEFAULT_BUSY_MILLIS),
            foreign_keys: std::cell::Cell::new(false),
            defer_foreign_keys: std::cell::Cell::new(false),
            locking_exclusive: std::cell::Cell::new(false),
            journal_mode: std::cell::Cell::new(inillucent_pool::journal::JournalMode::Delete),
            ignore_check_constraints: std::cell::Cell::new(false),
            secure_delete: std::cell::Cell::new(0),
            auto_vacuum: std::cell::Cell::new(0),
            automatic_index: std::cell::Cell::new(true),
            cache_size: std::cell::Cell::new(None),
            case_sensitive_like: std::cell::Cell::new(false),
            analysis_limit: std::cell::Cell::new(0),
            writable_schema: std::cell::Cell::new(false),
            defensive: std::cell::Cell::new(false),
            query_only: std::cell::Cell::new(false),
            recursive_triggers: std::cell::Cell::new(false),
            max_page_count: std::cell::Cell::new(crate::pragma::DEFAULT_MAX_PAGE_COUNT),
            temp_store: std::cell::Cell::new(0),
        }
    }
}

/// What one connection has that its siblings do not.
///
/// **One of the six groups `ImportedDatabase`'s fields are made of (task-1962,
/// A1 step 2).** The temporary objects, the attached databases, the connection
/// pragmas, the registered functions and collations, the modules, and the
/// authorizer. These are what "a connection" means in this engine, and A1 step 3
/// lifts them into `Session` - which is what makes two connections two things
/// rather than two numbers reaching into one.
pub(crate) struct SessionState {
    /// The authorizer every statement is bound under, when one is installed.
    ///
    /// `sqlite3_set_authorizer`'s subject: a callback the binder consults
    /// before it binds a read, a select or a function call, so an application
    /// embedding this engine can refuse a statement rather than run it. None
    /// means `AllowAll`, which is what a connection nobody has restricted has -
    /// and is the only case a compiled plan may be reused from the cache under,
    /// because re-running an authorizer is what makes its answer current.
    pub(crate) authorizer: Option<std::rc::Rc<dyn inillucent_sql::bind::Authorizer>>,
    /// The collations an application registered, by upper-cased name.
    ///
    /// The comparator itself lives in `inillucent-value`'s custom table, which
    /// is process-wide because a `Collation` is a `Copy` handle carried through
    /// every key and every comparison. What is per-connection is the *name*:
    /// two connections may register different comparators under `MYCOLL`, and
    /// the binder resolves the name against this list before it falls back to
    /// the built-ins.
    pub(crate) collations: Vec<(String, Collation)>,
    /// The modules this connection knows, which is the built-in set.
    ///
    /// Held rather than looked up per statement because a module is registered
    /// once and asked many times, and because `CREATE VIRTUAL TABLE` has to find
    /// one by name before anything else can happen.
    pub(crate) registry: inillucent_ext::registry::Registry,
    /// The eponymous virtual tables the registry provides, derived once.
    ///
    /// **A catalog refresh happens after every DDL statement, and deriving
    /// these means connecting every eponymous module to read its declaration.**
    /// They are a function of the registry alone - not of the schema - so
    /// re-deriving them per refresh was work with no input that had changed,
    /// on the path the gate's `schema.index` measures. Rebuilt only when a
    /// module or a pragma is registered, which is at open and nowhere else.
    pub(crate) eponymous: Vec<inillucent_sql::catalog_view::TableInfo>,
    /// The virtual tables that have been connected, by folded name.
    pub(crate) virtual_tables: HashMap<Vec<u8>, vtab::Connected>,
    /// The indexes a module owns, by the root page of the table they index.
    ///
    /// **A vector index is a store plus a promise to keep it in step.** The
    /// store is an ordinary `inillucent_search` virtual table; the promise is
    /// this map and the code in `write` that reads it. It is rebuilt whenever
    /// the catalog changes, from the `source=` argument the engine itself wrote
    /// when the index was created - so an index survives a close without a
    /// second schema to keep in step with the first.
    pub(crate) vector_indexes: HashMap<u32, Vec<VectorIndex>>,
    /// Whether the connected modules have been told this transaction started.
    ///
    /// `begin` fires once per write transaction that reaches a module, and this
    /// is what makes "once" true: `change_module` reads it before the first
    /// write and the commit and the rollback clear it. See
    /// `vtab::begin_modules`.
    pub(crate) modules_begun: std::cell::Cell<bool>,
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
    pub(crate) attached: Vec<Attached>,
    /// One temporary database per connection that has asked for one.
    ///
    /// **All at schema number one, told apart by whose they are.** A temporary
    /// object is one connection's own - `each_connection_has_its_own_temporary_database`
    /// grades exactly that against SQLite - so two connections' `temp.t` are two
    /// tables, and a statement reaches whichever belongs to the session running
    /// it.
    pub(crate) temps: Vec<Attached>,
    /// The session the statement now running belongs to.
    ///
    /// Set by every entry point from the connection that called it, so that
    /// `temp` resolves to that connection's temporary database and to no other.
    pub(crate) session: std::cell::Cell<u64>,
    /// The session `tables` and `catalog` were last derived for.
    ///
    /// **A connection's schema is its own.** `tables` holds the running
    /// session's temporary tables beside the shared ones, so when the session
    /// changes the derivation has to run again - once, on the change, rather
    /// than per statement. A connection that is the only one costs one
    /// comparison.
    pub(crate) tables_session: u64,
    /// Which schema each tree handle belongs to, for handles that are not
    /// `main`'s.
    ///
    /// Numbered as the binder numbers schemas: 0 is `main`, and *n* is
    /// `attached[n - 1]`. `main`'s handles are deliberately absent - a handle
    /// this map does not hold is `main`'s, which is what keeps a one-file
    /// connection's lookup a miss on an empty map rather than a hit on a full
    /// one.
    pub(crate) owner: HashMap<u32, usize>,
}

/// The catalog, the trees it names, and what was derived from both.
///
/// **One of the six groups `ImportedDatabase`'s fields are made of (task-1962,
/// A1 step 2).** Every one of these changes together, on a DDL statement and on
/// nothing else: the rows of `sqlite_schema`, the trees they name, the layouts and
/// covering sets derived from them, and the generation a cached plan is checked
/// against. A field here that moved without the others is a plan answering against
/// a schema that is not there any more, which is what `catalog_generation` exists
/// to catch.
pub(crate) struct Schema {
    pub(crate) catalog: StaticCatalog,
    /// How many times the catalog has changed.
    ///
    /// A plan compiled at one generation is not run at another: `execute_ddl`
    /// bumps this and empties the statement cache in the same breath, which is
    /// the TDD's "every plan cache is invalidated" made into two lines that
    /// cannot get out of step.
    pub(crate) catalog_generation: u64,
    pub(crate) trees: HashMap<u32, PagedTree>,
    pub(crate) layouts: HashMap<u32, std::rc::Rc<SourceLayout>>,
    /// For each table root, its index roots ordered smallest tree first.
    pub(crate) covering: HashMap<u32, Vec<u32>>,
    /// The catalog tree's rows, with what each one needs beside it.
    ///
    /// Held beside the tree rather than read back out of it on every DDL
    /// statement. The tree is the authority - it is what the file describes
    /// itself with, and `import_with` compares the two after the checkpoint -
    /// but a `DROP` has to find a row by name and the tree is keyed by rowid,
    /// so the alternative is a full scan per statement.
    pub(crate) entries: Vec<Recorded>,
    /// The tables the binder resolves names against, `sqlite_schema` excepted.
    ///
    /// **Derived from `entries`, always**, by `rebuild_tables`. Nothing adds a
    /// table here directly: a schema is one thing, and deriving it twice - once
    /// when a statement runs and once when the catalog is read back - is how the
    /// two come to disagree.
    pub(crate) tables: Vec<TableInfo>,
    /// `sqlite_schema`'s own declaration, re-registered on every rebuild.
    pub(crate) schema_info: TableInfo,
    /// The tables the import could not take, by name.
    pub(crate) skipped: Vec<String>,
    /// The identifier the next tree a DDL statement creates is registered under.
    ///
    /// Roots here are *identifiers*, not page numbers - the physical root is in
    /// the catalog row - and the imported ones are the fixture's SQLite page
    /// numbers, which start at 1 and count pages. So a DDL-created tree takes a
    /// number from the top half of the range, where no imported table can be,
    /// and `sqlite_schema` keeps `u32::MAX`.
    pub(crate) next_root: u32,
    /// The handle the next tree of an attached database is registered under.
    pub(crate) next_handle: u32,
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
    pub(crate) ddl_schema: usize,
    /// The imposter tables `.imposter` has made, and what each one reads.
    ///
    /// **Transient, and deliberately not in the catalog.** An imposter is a
    /// declaration over an index's own b-tree - `.imposter ix im` makes `im` a
    /// `WITHOUT ROWID` table whose columns are the index's entries - and it
    /// exists so a person can read an index directly when they are working out
    /// what is wrong with one. It is not a schema object: nothing writes it to
    /// the file, and it goes when the connection does, which is what SQLite's
    /// own `SQLITE_TESTCTRL_IMPOSTER` does with it.
    pub(crate) imposters: Vec<(TableInfo, SourceLayout, PagedTree)>,
}

/// The file, and everything that reads or writes a page of it.
///
/// **One of the six groups `ImportedDatabase`'s fields are made of (task-1962,
/// A1 step 2).** These are what an open database *is* before any statement runs:
/// the pager, the log, the path, the file system it was opened through, and the two
/// sizes both were opened with. Not one of them changes for the life of the handle,
/// which is what makes this the group A1 step 3 lifts into `Database`.
pub(crate) struct Storage {
    pub(crate) database: Database,
    /// The write-ahead log every change is described in before it happens.
    ///
    /// Held beside the file rather than inside an `inillucent-txn` `Engine`,
    /// because the read path takes `&Pool` as a plain borrow and an engine
    /// keeps its file behind a `RefCell` that cannot lend one. What this
    /// harness needs of a transaction manager is the log, the sync policy and
    /// the commit record; the snapshots and the version log are what the Phase
    /// 3 model driver exercises, and it drives the `Engine` directly.
    pub(crate) wal: std::rc::Rc<Wal>,
    /// The file the trees were written to, kept so it can be reported and
    /// cleaned up.
    pub(crate) path: PathBuf,
    pub(crate) page_size: usize,
    pub(crate) frames: usize,
    /// What opening this file did to it, for a caller that has to report it.
    ///
    /// Set once by the open and never changed: it describes the open, not the
    /// connection's later life. See `crate::recovery::RecoveryReport`.
    pub(crate) recovery: crate::recovery::RecoveryReport,
    /// Whether this file was opened with a transaction still in doubt.
    ///
    /// **A checkpoint must not fold the log down past a vote nobody has
    /// counted.** A transaction that wrote two files is decided by the deletion
    /// of a super-journal outside both, and until that deletion each
    /// participant's log holds a `Commit` record recovery is told to read as
    /// absent. A checkpoint that advanced this file's recovery point past those
    /// records would make the transaction unrecoverable whichever way the
    /// decision then went - so while the doubt is there, the recovery point
    /// does not move.
    ///
    /// Nothing reached it before task-1980 because under `locking_mode =
    /// exclusive` a connection checkpointed at close; with `normal` as the
    /// default, the `ATTACH` that a reader runs to look at both files
    /// checkpoints on its way out and did exactly this.
    pub(crate) in_doubt: bool,
    /// Whether this connection may write the file at all.
    ///
    /// See `inillucent_pool::Database::open_read_only`. Held here as well as on
    /// the pool so the engine can answer without a write having to fail first -
    /// a checkpoint on its way out of a statement, for instance, has nothing to
    /// do and should not report a refusal nobody asked for.
    pub(crate) read_only: bool,
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
    pub(crate) vfs: std::sync::Arc<dyn inillucent_vfs::Vfs>,
}

/// What a transaction in progress has done so far.
///
/// **One of the six groups `ImportedDatabase`'s fields are made of (task-1962,
/// A1 step 2).** The batch, what it has touched, what it can be undone to, and the
/// flags that say whether one is open and who opened it. A1 step 3 lifts this into
/// `Writer`, which is the type that makes "one transaction at a time" something the
/// compiler knows rather than a sentence in a doc comment.
/// Where a statement's writes begin, in each of a transaction's records.
///
/// **One value because they are taken together and used together.** A statement
/// that fails is rolled back to the undo buffer's length *and* to the
/// pending-free list's length, and passing them as two integers grew
/// `ImportedDatabase::write` past the length it is recorded at - which is the
/// ratchet doing its job: the second one belongs with the first, not beside it.
/// The third arrived with task-2065 and went the same way rather than beside
/// it, which is why `undo_to_floor` takes this value now instead of a list of
/// integers that grows every time a transaction learns to record something
/// else.
#[derive(Clone, Copy)]
pub(crate) struct StatementMark {
    /// How long the undo buffer was.
    pub(crate) undo: usize,
    /// How long the pending-free list was.
    pub(crate) dropped: usize,
    /// How long the list of trees this transaction has built was.
    pub(crate) built: usize,
}

/// One thing a transaction has dropped and has not yet committed.
///
/// See [`Writing::pending_frees`] for why a drop waits: the free map is durable
/// state, so giving a page back before the transaction that dropped it has
/// committed is a change that a rollback would have to take back, and nothing in
/// this engine's row-level undo buffer can.
///
/// The schema travels with each entry because one transaction may drop tables in
/// more than one attached database, and a page number alone does not say which
/// file's free map it belongs to.
pub(crate) struct PendingFree {
    /// Which attached database it belongs to.
    pub(crate) schema: usize,
    /// What is being given back.
    pub(crate) what: Freed,
}

/// What one entry of the pending-free list gives back.
///
/// **A dropped tree gives back two kinds of thing and only one of them is a
/// page number (task-2065).** A page of the tree itself belongs to that tree
/// alone, so the commit hands it straight to the free map. A page an
/// out-of-line value sits on may be shared with values from other trees, and
/// giving it back because this tree used it would free a page another tree's
/// value is still on - so what is recorded for a value is the reference, and
/// `paged::free_extent` decides at the commit whether the page goes with it.
pub(crate) enum Freed {
    /// A page of the dropped tree - an interior page or a leaf.
    Page(inillucent_pool::PageId),
    /// One out-of-line value of the dropped tree, as its leaf held it.
    Value(inillucent_pool::extent::ExtentRef),
}

/// One tree an open transaction has built and has not yet committed.
///
/// **The record that tells a rolled-back `CREATE` apart from a `DROP` whose
/// pages belong to the commit (task-2065).** Both leave a handle the schema no
/// longer names, and the two want opposite things done with the pages: a tree
/// this transaction built was never committed, so an abandoned transaction has
/// to give its pages back; a tree it dropped is on the pending-free list and
/// its pages belong to the commit, which an abandoned transaction never
/// reaches. Guessing from the catalog cannot separate them, because after the
/// undo neither is in it.
///
/// The root page is recorded rather than only the handle, because by the time
/// the rollback reads this the handle may hold nothing - the tree was dropped
/// later in the same transaction - or may hold a different tree entirely, which
/// is what `ALTER TABLE` leaves behind when it rebuilds under the same handle.
/// The page is what still identifies the tree in both cases.
pub(crate) struct BuiltTree {
    /// Which attached database the tree was built in.
    pub(crate) schema: usize,
    /// The handle it was registered under.
    pub(crate) root: u32,
    /// The page its root sits on.
    pub(crate) page: inillucent_pool::PageId,
}

pub(crate) struct Writing {
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
    ///
    /// **Behind a cell (task-1962, A1 step 3).** Every other field of this
    /// group already was, and the two that were not are what made the whole
    /// group need `&mut`. With all ten behind their own cells the group is
    /// reachable through a shared reference, which is what lets a connection
    /// hold the writer without borrowing the engine.
    touched: std::cell::Cell<u16>,
    /// Named savepoints, and where each one sits in `undo` and in
    /// `pending_frees`.
    ///
    /// Behind a cell for the reason `touched` is, and it is a `RefCell` rather
    /// than a `Cell` because the list is read in place - `release` finds a name
    /// in it - and copying it to read one entry would allocate per savepoint
    /// statement.
    ///
    /// **A whole [`StatementMark`], because a transaction has several
    /// append-only records of what it has done** and rolling back to a
    /// savepoint has to cut every one of them to where it stood when the
    /// savepoint was taken. Carrying only the `undo` length and deriving the
    /// others from it is what a first version did, and it is wrong at the
    /// boundary: a `DROP` and a `SAVEPOINT` taken immediately after it sit at
    /// the same `undo` length, so nothing in that number says which came
    /// first.
    marks: std::cell::RefCell<Vec<(Vec<u8>, StatementMark)>>,
    /// Pages the open transaction has dropped, waiting for its commit.
    ///
    /// **A page is given back to the free map at commit, not at the statement
    /// that dropped it (task-2043).** The free map is shared, durable state, and
    /// `inillucent_pool::FreeMap::free` rewinds the allocator's hint down to the
    /// page it just freed - so a `DROP TABLE` that freed a page mid transaction
    /// had it handed straight back to the next `CREATE TABLE`, which wrote an
    /// empty root over it. Rolling back then restored the dropped table's
    /// catalog row, `reattach_entries` attached its tree at the root page that
    /// row still names, and the table came back empty. Durably, because the
    /// rows really were gone.
    ///
    /// The undo buffer cannot repair that: it holds row before-images, not page
    /// images, so there is nothing in it that says what page P used to contain.
    /// Holding the frees until the commit means the question never arises - a
    /// transaction that is abandoned never gave the page away.
    ///
    /// It is also what makes a `DROP` that is rolled back leave the free map
    /// alone. Before this, `BEGIN; DROP TABLE p; ROLLBACK` answered `3` and left
    /// page P marked free while p still pointed at it, so the *next* statement
    /// to allocate anything overwrote p's rows.
    pending_frees: std::cell::RefCell<Vec<PendingFree>>,
    /// Trees the open transaction has built, waiting for its commit.
    ///
    /// **The other half of task-2043's rule, and the leak it left (task-2065).**
    /// Holding a drop's frees until the commit is what stops a rollback losing
    /// rows; it says nothing about an allocation the rollback abandons. A
    /// `CREATE TABLE` or `CREATE INDEX` inside a transaction allocates a root
    /// page, and the undo buffer is row-level - it replays before-images
    /// through `tree.put` and `tree.delete`, and no before-image says a page
    /// was once free. So the page stayed marked allocated with nothing naming
    /// it, one page per rolled-back `CREATE`.
    ///
    /// It is invisible on the connection that did it, because the rollback
    /// leaves the tree's handle in `schema.trees` and a page walk therefore
    /// still reaches the page. It appears after a checkpoint and a reopen, when
    /// the schema is rebuilt from the catalog and nothing names that tree.
    ///
    /// Cleared at every commit, because a committed allocation is not one
    /// anybody takes back.
    built: std::cell::RefCell<Vec<BuiltTree>>,
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
    /// How many statements are running, for the file lock.
    ///
    /// A statement runs statements - a trigger body, a foreign-key sweep, a
    /// `CHECK` - so the lock is taken on the way into the outermost one and
    /// released on the way out of it. A counter rather than a flag because the
    /// nesting is real and an inner release would drop the file while the outer
    /// statement was still reading it.
    ///
    /// Behind a cell for the reason `touched` is.
    running: std::cell::Cell<usize>,
    /// Whether a cyclic-key sweep is already running.
    ///
    /// The sweep runs statements, and a statement runs the sweep; without this
    /// the first cascade would recur until the stack ran out. It is a flag
    /// rather than a depth because there is exactly one sweep at a time by
    /// construction: it runs after a statement, at the outermost level.
    settling: std::cell::Cell<bool>,
}

/// The compiled statements this connection is holding on to.
///
/// **One of the six groups `ImportedDatabase`'s fields are made of (task-1962,
/// A1 step 2).** A cache, its bound, what it has cost, and the two scratch buffers a
/// compile reuses. Every one of them is an optimization: dropping the whole group
/// changes how fast the engine answers and not what it answers, which is exactly
/// what `PRAGMA plan_cache` and the `PLAN_CACHE` lever already do.
pub(crate) struct Compiled {
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
    /// **And the parameter count beside it** (task-2066 §4.3.3). It is what
    /// `sqlite3_bind_parameter_count` answers and what a bind is checked
    /// against, and it comes out of the same parse that produced the plan -
    /// so a caller wanting both used to ask twice and parse twice. `Cached`
    /// has thirteen variants and no place to put it, which is why it is here
    /// rather than on the plan.
    pub(crate) statements:
        std::cell::RefCell<HashMap<u64, HashMap<String, (std::rc::Rc<Cached>, u32)>>>,
    /// The plan cache's ceiling; see `plans.rs`, which holds and enforces it.
    pub(crate) statement_cache_limit: std::cell::Cell<usize>,
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
    pub(crate) compiles: std::cell::Cell<u64>,
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
    pub(crate) scratch_ast: std::cell::RefCell<Option<inillucent_sql::ast::Ast>>,
    /// One set of binder vectors, kept and cleared rather than made per
    /// statement.
    ///
    /// **The same argument as `scratch_ast`, one stage later (task-2026).**
    /// Binding `SELECT 1` took the scope stack's buffer and the result-alias
    /// buffer out of the allocator every time - 96 and 320 bytes, two of the
    /// twenty-one allocations the gate's `prepare.trivial` iteration makes -
    /// to build vectors thrown away a microsecond later.
    ///
    /// It is taken out on the way in and put back on the way out, so a nested
    /// bind - a view body, a trigger - finds the cell empty and makes its own
    /// rather than clearing the vectors the outer statement is holding names
    /// in.
    pub(crate) scratch_binder: std::cell::RefCell<Option<inillucent_sql::bind::BinderScratch>>,
    /// Where the last `CREATE INDEX` spent its time, in nanoseconds.
    ///
    /// Scan, sort, uniqueness check, pack. On the harness's own type, in a
    /// test-only crate, and nothing in the engine consults it - the same shape
    /// as the write path's `execute_timed`, and for the same reason: `schema`
    /// is a gate this project has already been wrong about the cause of once.
    pub(crate) index_stages: std::cell::Cell<StageTimings>,
}

/// What the engine remembers about statements that have already run.
///
/// **One of the six groups `ImportedDatabase`'s sixty-eight fields are made of
/// (task-1962, A1 step 2).** These five are read by `last_insert_rowid()`,
/// `changes()`, `total_changes()` and the random built-ins, and by nothing else.
/// Naming them as a group is what lets a reader see that a statement's *result* is
/// five cells and not sixty-eight.
pub(crate) struct Counters {
    /// The rowid the last `INSERT` assigned, for `last_insert_rowid`.
    ///
    /// **Deliberately not restored by a rollback.** SQLite documents the value
    /// as the last rowid *attempted*, and `faults.rs` pins that: an insert that
    /// is rolled back still moves it. Restoring it would be a different answer
    /// wearing the same name.
    pub(crate) last_rowid: std::cell::Cell<i64>,
    /// Every row every statement on this database has changed.
    ///
    /// A trigger's rows and a foreign key's cascade are in it, which is
    /// SQLite's rule and is the difference between this and `last_changes`.
    /// Never decremented: a `ROLLBACK` does not put it back, which was measured
    /// against the pinned shell rather than assumed.
    pub(crate) changed_ever: std::cell::Cell<i64>,
    /// What `changed_ever` read when each connection's session was opened, so
    /// `total_changes()` answers for this session alone rather than for every
    /// session this database has ever handed out. See
    /// [`session_changes::SessionChanges`].
    pub(crate) session_change_baseline: session_changes::SessionChanges,
    /// How many rows the most recent write changed, for `changes()`.
    ///
    /// The statement's own rows only - a trigger body's are not in it. A
    /// statement that changed nothing sets it to zero; a `SELECT`, a DDL and a
    /// transaction statement leave it alone.
    pub(crate) last_changes: std::cell::Cell<i64>,
    /// The random built-ins' stream, advanced once per statement.
    pub(crate) seed: std::cell::Cell<u64>,
}

impl Schema {
    /// Reports whether any key can lead back to the table that declares it.
    pub(crate) fn has_cyclic_foreign_keys(&self) -> bool {
        self.tables
            .iter()
            .any(|table| table.foreign_keys.iter().any(|key| key.cyclic))
    }

    /// One foreign key's violation query, with what it is about.
    ///
    /// The child and parent names and the key's own id are carried alongside
    /// the SQL because `PRAGMA foreign_key_check` reports all three and the
    /// query itself only produces a rowid.
    pub(crate) fn violation_queries(&self, only: Option<&str>) -> DbResult<Vec<ViolationQuery>> {
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
}

impl SessionState {
    /// Returns the schema one number names, for the session now running.
    ///
    /// `None` for `main`, which is held as this type's own fields rather than as
    /// an element, and for a number nothing holds.
    ///
    /// @param at - the schema, as the binder numbers them
    pub(crate) fn schema_at(&self, at: usize) -> Option<&Attached> {
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
    pub(crate) fn schema_at_mut(&mut self, at: usize) -> Option<&mut Attached> {
        let session = self.session.get();
        schema_of_index(&mut self.attached, &mut self.temps, session, at)
    }

    /// Returns which schema a tree handle belongs to; `MAIN` when it is
    /// `main`'s.
    ///
    /// **A handle this connection has not attached anything under is `main`'s.**
    /// The map holds only the handles of attached databases, so a connection
    /// that has attached nothing answers without hashing anything at all - which
    /// is what keeps this off the read path's bill.
    ///
    /// @param root - the handle
    pub(crate) fn schema_of(&self, root: u32) -> usize {
        if self.attached.is_empty() && self.temps.is_empty() {
            return MAIN;
        }
        self.owner.get(&root).copied().unwrap_or(MAIN)
    }
}

// The accessors. See the note above `impl Pragmas` for why the fields are
// private; the `RefCell` ones hand back the cell rather than a borrow of it,
// because six call sites pass `&self.writing.undo` into a `WriteView` and a
// method returning a `Ref` cannot stand in for that.

impl Writing {
    /// Returns the cell holding marks.
    ///
    /// The cell rather than a borrow of it, because a caller that hands this to
    /// another type needs the cell itself.
    pub(crate) fn marks(&self) -> &std::cell::RefCell<Vec<(Vec<u8>, StatementMark)>> {
        &self.marks
    }

    /// Returns the cell holding the pages waiting to be freed at commit.
    ///
    /// The cell rather than a borrow of it, for the reason [`Writing::marks`]
    /// hands back the cell.
    pub(crate) fn pending_frees(&self) -> &std::cell::RefCell<Vec<PendingFree>> {
        &self.pending_frees
    }

    /// Returns the cell holding the trees this transaction has built.
    ///
    /// The cell rather than a borrow of it, for the reason [`Writing::marks`]
    /// hands back the cell.
    pub(crate) fn built(&self) -> &std::cell::RefCell<Vec<BuiltTree>> {
        &self.built
    }

    /// Returns the cell holding undo.
    ///
    /// The cell rather than a borrow of it, because a caller that hands this to
    /// another type needs the cell itself.
    pub(crate) fn undo(&self) -> &std::cell::RefCell<Vec<Before>> {
        &self.undo
    }

    /// Returns the write state a freshly opened database starts with.
    ///
    /// A constructor rather than a literal at each open path, because the
    /// fields are private and because the two paths - a new file and a
    /// recovered one - differ in exactly one value and used to restate the
    /// other nine each.
    ///
    /// @param next_txn - the first transaction number this database will hand out
    pub(crate) fn starting_at(next_txn: u64) -> Writing {
        Writing {
            next_txn: std::cell::Cell::new(next_txn),
            statement_txn: std::cell::Cell::new(None),
            batch: std::cell::Cell::new(None),
            undo: std::cell::RefCell::new(Vec::new()),
            touched: std::cell::Cell::new(0),
            decided_over: std::cell::Cell::new(0),
            marks: std::cell::RefCell::new(Vec::new()),
            pending_frees: std::cell::RefCell::new(Vec::new()),
            built: std::cell::RefCell::new(Vec::new()),
            implicit_transaction: std::cell::Cell::new(false),
            running: std::cell::Cell::new(0),
            settling: std::cell::Cell::new(false),
        }
    }

    /// Sets touched and returns what it was.
    ///
    /// @param value - what to set it to
    pub(crate) fn replace_touched(&self, value: u16) -> u16 {
        self.touched.replace(value)
    }

    /// Takes the batch's transaction number, leaving none behind.
    pub(crate) fn take_batch(&self) -> Option<u64> {
        self.batch.take()
    }

    /// Returns batch.
    pub(crate) fn batch(&self) -> Option<u64> {
        self.batch.get()
    }

    /// Sets batch.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_batch(&self, value: Option<u64>) {
        self.batch.set(value);
    }

    /// Returns decided over.
    pub(crate) fn decided_over(&self) -> usize {
        self.decided_over.get()
    }

    /// Sets decided over.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_decided_over(&self, value: usize) {
        self.decided_over.set(value);
    }

    /// Returns implicit transaction.
    pub(crate) fn implicit_transaction(&self) -> bool {
        self.implicit_transaction.get()
    }

    /// Sets implicit transaction.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_implicit_transaction(&self, value: bool) {
        self.implicit_transaction.set(value);
    }

    /// Returns next txn.
    pub(crate) fn next_txn(&self) -> u64 {
        self.next_txn.get()
    }

    /// Sets next txn.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_next_txn(&self, value: u64) {
        self.next_txn.set(value);
    }

    /// Returns running.
    pub(crate) fn running(&self) -> usize {
        self.running.get()
    }

    /// Sets running.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_running(&self, value: usize) {
        self.running.set(value);
    }

    /// Returns settling.
    pub(crate) fn settling(&self) -> bool {
        self.settling.get()
    }

    /// Sets settling.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_settling(&self, value: bool) {
        self.settling.set(value);
    }

    /// Returns statement txn.
    pub(crate) fn statement_txn(&self) -> Option<u64> {
        self.statement_txn.get()
    }

    /// Sets statement txn.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_statement_txn(&self, value: Option<u64>) {
        self.statement_txn.set(value);
    }

    /// Returns touched.
    pub(crate) fn touched(&self) -> u16 {
        self.touched.get()
    }

    /// Sets touched.
    ///
    /// @param value - what to set it to
    pub(crate) fn set_touched(&self, value: u16) {
        self.touched.set(value);
    }
}

impl Writing {
    /// Takes every field from a freshly opened connection's group, keeping this
    /// group's identity.
    ///
    /// **For the one place that replaces a whole `ImportedDatabase`
    /// (task-1962, A1 step 3).** `VACUUM` reopens the file by assigning a
    /// freshly opened engine over the connection, and
    /// [`crate::connect::Database`] holds a second handle on the writer the
    /// connection was built with. A fresh group there left that handle reading
    /// a writer nothing writes to, so `autocommit()` answered `true` inside a
    /// `BEGIN`; the differential suite's `vacuum_matches_sqlite` is what found
    /// it. Copying rather than swapping keeps both handles on one group and
    /// leaves the values exactly what the reopen decided.
    ///
    /// @param fresh - the group the reopened connection built
    pub(crate) fn adopt(&self, fresh: &Writing) {
        self.batch.set(fresh.batch.get());
        self.undo.replace(fresh.undo.take());
        self.touched.set(fresh.touched.get());
        self.marks.replace(fresh.marks.take());
        self.decided_over.set(fresh.decided_over.get());
        self.implicit_transaction
            .set(fresh.implicit_transaction.get());
        self.next_txn.set(fresh.next_txn.get());
        self.statement_txn.set(fresh.statement_txn.get());
        self.running.set(fresh.running.get());
        self.settling.set(fresh.settling.get());
    }
}

impl Compiled {
    /// Returns how many compiled statements this connection is holding.
    ///
    /// Counted across every session, because the cache is keyed by session and
    /// a caller asking how much it is holding means all of it.
    pub(crate) fn held(&self) -> usize {
        self.statements
            .borrow()
            .values()
            .map(HashMap::len)
            .fold(0usize, usize::saturating_add)
    }

    /// Returns the ceiling one session's cache is emptied at.
    pub(crate) fn limit(&self) -> usize {
        self.statement_cache_limit.get()
    }

    /// Sets the ceiling one session's cache is emptied at.
    ///
    /// Zero means every statement is compiled fresh, which is what a caller
    /// diagnosing a plan wants and what nothing else should ask for.
    ///
    /// @param most - how many compiled statements one session may hold
    pub(crate) fn set_limit(&self, most: usize) {
        self.statement_cache_limit.set(most);
        if most == 0 {
            self.statements.borrow_mut().clear();
        }
    }

    /// Forgets every compiled statement.
    pub(crate) fn forget_all(&self) {
        self.statements.borrow_mut().clear();
    }

    /// Returns how many statements this connection has compiled since it
    /// opened.
    pub(crate) fn compiles(&self) -> u64 {
        self.compiles.get()
    }

    /// Takes every field from a freshly opened connection's cache, keeping this
    /// cache's identity.
    ///
    /// The counterpart of [`Writing::adopt`], and for the same swap. It matters
    /// more here: a `VACUUM` gives every tree a fresh root, so a plan compiled
    /// before it names a tree that is no longer there. The reopened connection
    /// builds an empty cache and this is how the group both handles hold
    /// becomes that empty cache rather than keeping plans the rebuild invalidated.
    ///
    /// @param fresh - the cache the reopened connection built
    pub(crate) fn adopt(&self, fresh: &Compiled) {
        self.statements.replace(fresh.statements.take());
        self.statement_cache_limit
            .set(fresh.statement_cache_limit.get());
        self.compiles.set(fresh.compiles.get());
        self.scratch_ast.replace(fresh.scratch_ast.take());
        self.scratch_binder.replace(fresh.scratch_binder.take());
        self.index_stages.set(fresh.index_stages.get());
    }
}

impl Compiled {
    /// Puts a finished parse's arena back for the next statement to fill.
    ///
    /// @param parsed - the parse nothing holds a reference into any more
    pub(crate) fn recycle(&self, parsed: inillucent_sql::parser::ParsedStatement) {
        *self.scratch_ast.borrow_mut() = Some(parsed.ast);
    }

    /// Takes the binder's vectors out for one bind, leaving the cell empty.
    ///
    /// Empty rather than shared, for the reason [`Compiled::scratch_ast`]
    /// gives: a bind that starts while another is running - a view's body, a
    /// trigger - gets its own vectors rather than clearing the ones the outer
    /// statement is still holding names in.
    pub(crate) fn take_binder_scratch(&self) -> inillucent_sql::bind::BinderScratch {
        self.scratch_binder
            .borrow_mut()
            .take()
            .unwrap_or_else(inillucent_sql::bind::BinderScratch::new)
    }

    /// Puts a finished bind's vectors back for the next statement to fill.
    ///
    /// @param scratch - the vectors nothing holds a reference into any more
    pub(crate) fn recycle_binder(&self, scratch: inillucent_sql::bind::BinderScratch) {
        *self.scratch_binder.borrow_mut() = Some(scratch);
    }
}

impl Writing {
    /// Starts this connection's transaction counter above a number a log holds.
    ///
    /// **One counter, and now more than one log.** A file this connection
    /// attaches may hold higher transaction numbers than anything it has
    /// issued, and a number reused across the two would make a crashed run's
    /// records replay under a live transaction's commit - the resurrection
    /// `inillucent-wal` documents `Recovered::highest_txn` for.
    ///
    /// @param highest - the highest number the file's log carries
    pub(crate) fn raise_transactions_past(&self, highest: u64) {
        let next = highest.saturating_add(1);
        if self.next_txn.get() < next {
            self.next_txn.set(next);
        }
    }
}
