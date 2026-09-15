//! What a virtual table module is told about the transaction around it.
//!
//! Invariant: **a module hears every moment of the transaction it is inside.**
//! Five of them - `begin`, `savepoint`, `release`, `rollback`, `rollback_to` -
//! and a module that buffers writes needs the first three to know when a buffer
//! starts, when a point inside it was marked, and when that point stopped
//! mattering.
//!
//! ## The defect (task-1932, M2)
//!
//! The engine called `begin` exactly once, at `CREATE VIRTUAL TABLE`, and never
//! called `savepoint` or `release` at all. So:
//!
//! - a module could not buffer a transaction's writes, because there was no
//!   moment at which one started;
//! - a module could not stage anything under a savepoint, because it never
//!   heard about one;
//! - FTS5's own `begin` gates on `self.creating` and does nothing afterwards,
//!   which is what a module writes when the hook only ever fires at creation.
//!
//! `docs/roadmap.md` item 6 names these as the prerequisite for caching a
//! manifest, which is why no module caches one.
//!
//! ## What is counted
//!
//! A module that counts every call and nothing else, so the assertions are
//! about the sequence rather than about anything the module did with it. Two
//! write transactions and one `SAVEPOINT`/`RELEASE` pair give 2, 1, 1. Before
//! this ticket they gave 1, 0, 0 - and the 1 was the `CREATE`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use inillucent_compat::differential::scratch;
use inillucent_engine::connect::{Connection, Database};
use inillucent_engine::ext::vtab::FilterPlan;
use inillucent_engine::ext::vtab::{
    Change, Context, Declaration, DeclaredColumn, IndexQuery, Module, ModuleArguments, ShadowTable,
    VirtualCursor, VirtualTable,
};
use inillucent_value::value::Value;

/// Where this suite's scratch databases live.
const AREA: &str = "vtab-lifecycle";

/// Every call the module has been told about, by moment.
///
/// Shared with the test rather than read back through SQL, because what is
/// being asserted is the *call*, and a module that did something observable on
/// each call would be asserting the something instead.
#[derive(Default)]
struct Counts {
    begin: AtomicUsize,
    savepoint: AtomicUsize,
    release: AtomicUsize,
    rollback: AtomicUsize,
    rollback_to: AtomicUsize,
    commit: AtomicUsize,
    sync: AtomicUsize,
    schema_changed: AtomicUsize,
    committed_elsewhere: AtomicUsize,
}

impl Counts {
    /// Returns one counter's value.
    ///
    /// @param counter - the counter to read
    fn read(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::Relaxed)
    }
}

/// A module whose tables count what they are told and hold no rows.
struct CountingModule {
    counts: Arc<Counts>,
}

impl Module for CountingModule {
    fn name(&self) -> &str {
        "counting"
    }

    fn shadow_tables(
        &self,
        _arguments: &ModuleArguments,
    ) -> inillucent_base::DbResult<Vec<ShadowTable>> {
        Ok(Vec::new())
    }

    fn connect(
        &self,
        _arguments: &ModuleArguments,
        _creating: bool,
    ) -> inillucent_base::DbResult<Box<dyn VirtualTable>> {
        Ok(Box::new(CountingTable {
            declaration: Declaration {
                columns: vec![DeclaredColumn::visible("body")],
                without_rowid: false,
            },
            counts: Arc::clone(&self.counts),
        }))
    }
}

/// One connected counting table.
struct CountingTable {
    declaration: Declaration,
    counts: Arc<Counts>,
}

impl VirtualTable for CountingTable {
    fn declaration(&self) -> &Declaration {
        &self.declaration
    }

    fn best_index(&self, query: &mut IndexQuery) -> inillucent_base::DbResult<()> {
        query.estimated_cost = 1.0;
        query.estimated_rows = 0;
        Ok(())
    }

    fn open(&self) -> inillucent_base::DbResult<Box<dyn VirtualCursor>> {
        Ok(Box::new(EmptyCursor))
    }

    /// Accepts every write and keeps nothing.
    ///
    /// A module that refused would never reach the moments this suite counts,
    /// and one that stored rows would be asserting storage.
    fn update(
        &mut self,
        _context: &mut Context<'_>,
        _change: &Change,
    ) -> inillucent_base::DbResult<Option<i64>> {
        Ok(Some(1))
    }

    fn begin(&mut self, _context: &mut Context<'_>) -> inillucent_base::DbResult<()> {
        self.counts.begin.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn sync(&mut self, _context: &mut Context<'_>) -> inillucent_base::DbResult<()> {
        self.counts.sync.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn commit(&mut self, _context: &mut Context<'_>) -> inillucent_base::DbResult<()> {
        self.counts.commit.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn rollback(&mut self, _context: &mut Context<'_>) -> inillucent_base::DbResult<()> {
        self.counts.rollback.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn savepoint(
        &mut self,
        _context: &mut Context<'_>,
        _number: i32,
    ) -> inillucent_base::DbResult<()> {
        self.counts.savepoint.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn release(
        &mut self,
        _context: &mut Context<'_>,
        _number: i32,
    ) -> inillucent_base::DbResult<()> {
        self.counts.release.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn rollback_to(
        &mut self,
        _context: &mut Context<'_>,
        _number: i32,
    ) -> inillucent_base::DbResult<()> {
        self.counts.rollback_to.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn schema_changed(&mut self) {
        self.counts.schema_changed.fetch_add(1, Ordering::Relaxed);
    }

    fn committed_elsewhere(&mut self) {
        self.counts
            .committed_elsewhere
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// A cursor over no rows.
struct EmptyCursor;

impl VirtualCursor for EmptyCursor {
    fn filter(
        &mut self,
        _context: &mut Context<'_>,
        _plan: &FilterPlan,
    ) -> inillucent_base::DbResult<()> {
        Ok(())
    }

    fn next(&mut self, _context: &mut Context<'_>) -> inillucent_base::DbResult<()> {
        Ok(())
    }

    fn eof(&self) -> bool {
        true
    }

    fn column(
        &mut self,
        _context: &mut Context<'_>,
        _index: usize,
    ) -> inillucent_base::DbResult<Value<'static>> {
        Ok(Value::Null)
    }

    fn rowid(&self) -> inillucent_base::DbResult<i64> {
        Ok(0)
    }
}

/// Runs a statement for its effect.
fn exec(connection: &Connection<'_>, sql: &str) {
    connection
        .execute_batch(sql)
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()));
}

/// Opens a database with the counting module registered and one table made.
///
/// @param name - the file's name
fn counted(name: &str) -> (Connection<'static>, Arc<Counts>) {
    let counts = Arc::new(Counts::default());
    // Opened here rather than through `start_inillucent`, because a module has
    // to be registered on the *database* before the first connection uses it
    // and that helper hands back only the connection. Leaked for the same
    // reason it is: a `Connection` borrows its database, and a test that owned
    // both would have to thread two lifetimes through every case.
    let path = scratch(AREA, name, "inillucent");
    let database: &'static Database =
        Box::leak(Box::new(Database::open(&path).expect("the database opens")));
    database
        .register_module(Arc::new(CountingModule {
            counts: Arc::clone(&counts),
        }))
        .expect("the module registers");
    let connection = database.session();
    exec(&connection, "CREATE VIRTUAL TABLE t USING counting");
    (connection, counts)
}

/// Two write transactions begin twice, and a savepoint pair is heard once each.
///
/// **The case the TDD names: 2, 1, 1 where it used to be 1, 0, 0.** The one was
/// the `CREATE VIRTUAL TABLE`, which is why the count is taken after the create
/// rather than from zero.
#[test]
fn two_write_transactions_and_a_savepoint_pair_are_each_heard_once() {
    let (connection, counts) = counted("moments");
    let after_create = Counts::read(&counts.begin);

    exec(&connection, "BEGIN");
    exec(&connection, "INSERT INTO t (body) VALUES ('first')");
    exec(&connection, "COMMIT");

    exec(&connection, "BEGIN");
    exec(&connection, "INSERT INTO t (body) VALUES ('second')");
    exec(&connection, "SAVEPOINT inner_point");
    exec(&connection, "INSERT INTO t (body) VALUES ('third')");
    exec(&connection, "RELEASE inner_point");
    exec(&connection, "COMMIT");

    assert_eq!(
        Counts::read(&counts.begin).saturating_sub(after_create),
        2,
        "two write transactions produced {} begins",
        Counts::read(&counts.begin).saturating_sub(after_create)
    );
    assert_eq!(
        Counts::read(&counts.savepoint),
        1,
        "one SAVEPOINT produced {} calls",
        Counts::read(&counts.savepoint)
    );
    assert_eq!(
        Counts::read(&counts.release),
        1,
        "one RELEASE produced {} calls",
        Counts::read(&counts.release)
    );
}

/// `begin` fires once per transaction however many statements write.
///
/// The flag that makes "once" true is the thing this asserts: three writes in
/// one transaction are one transaction.
#[test]
fn begin_fires_once_per_transaction_not_once_per_statement() {
    let (connection, counts) = counted("once");
    let after_create = Counts::read(&counts.begin);

    exec(&connection, "BEGIN");
    for nth in 0..5 {
        exec(
            &connection,
            &format!("INSERT INTO t (body) VALUES ('row {nth}')"),
        );
    }
    exec(&connection, "COMMIT");

    assert_eq!(
        Counts::read(&counts.begin).saturating_sub(after_create),
        1,
        "five statements in one transaction produced {} begins",
        Counts::read(&counts.begin).saturating_sub(after_create)
    );
}

/// A transaction that writes nothing to a module does not begin one on it.
///
/// **"Every write transaction that reaches a module", and a transaction that
/// writes only ordinary tables does not reach one.** Beginning on every
/// transaction would make a module buffer for statements it never sees.
#[test]
fn a_transaction_that_does_not_reach_the_module_does_not_begin_one() {
    let (connection, counts) = counted("untouched");
    exec(
        &connection,
        "CREATE TABLE ordinary (id INTEGER PRIMARY KEY)",
    );
    let after_create = Counts::read(&counts.begin);

    exec(&connection, "BEGIN");
    exec(&connection, "INSERT INTO ordinary VALUES (1)");
    exec(&connection, "COMMIT");

    assert_eq!(
        Counts::read(&counts.begin).saturating_sub(after_create),
        0,
        "a transaction that never touched the module began one on it"
    );
}

/// An abandoned transaction is rolled back on the module, and a later one
/// begins again.
#[test]
fn a_rollback_is_heard_and_the_next_transaction_begins_again() {
    let (connection, counts) = counted("rolled-back");
    let after_create = Counts::read(&counts.begin);

    exec(&connection, "BEGIN");
    exec(&connection, "INSERT INTO t (body) VALUES ('discarded')");
    exec(&connection, "ROLLBACK");

    assert_eq!(
        Counts::read(&counts.rollback),
        1,
        "the abandoned transaction produced {} rollbacks",
        Counts::read(&counts.rollback)
    );

    exec(&connection, "BEGIN");
    exec(&connection, "INSERT INTO t (body) VALUES ('kept')");
    exec(&connection, "COMMIT");

    assert_eq!(
        Counts::read(&counts.begin).saturating_sub(after_create),
        2,
        "the transaction after a rollback did not begin one on the module"
    );
}

/// `ROLLBACK TO` is heard with the level of the savepoint it returns to.
#[test]
fn a_rollback_to_a_savepoint_is_heard() {
    let (connection, counts) = counted("rollback-to");

    exec(&connection, "BEGIN");
    exec(&connection, "INSERT INTO t (body) VALUES ('kept')");
    exec(&connection, "SAVEPOINT point");
    exec(&connection, "INSERT INTO t (body) VALUES ('discarded')");
    exec(&connection, "ROLLBACK TO point");
    exec(&connection, "COMMIT");

    assert_eq!(Counts::read(&counts.savepoint), 1);
    assert_eq!(
        Counts::read(&counts.rollback_to),
        1,
        "ROLLBACK TO produced {} calls",
        Counts::read(&counts.rollback_to)
    );
}

/// A schema change is told to the module.
///
/// **The hook `docs/roadmap.md` item 6 names as a prerequisite for caching a
/// manifest.** A module holding anything derived from the catalog has no other
/// moment at which to drop it.
#[test]
fn a_schema_change_is_told_to_the_module() {
    let (connection, counts) = counted("schema");
    let before = Counts::read(&counts.schema_changed);
    exec(&connection, "CREATE TABLE later (id INTEGER PRIMARY KEY)");
    assert!(
        Counts::read(&counts.schema_changed) > before,
        "a CREATE TABLE did not reach the module's schema_changed"
    );
}
