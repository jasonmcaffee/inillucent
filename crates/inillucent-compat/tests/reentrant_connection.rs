//! Calling a connection from inside something the connection is running.
//!
//! Invariant: **a reentrant call is refused, not fatal.** The whole engine sits
//! behind one `RefCell` (`crates/inillucent-engine/src/connect.rs`), so a
//! callback that reaches back into the connection that invoked it asks for a
//! borrow that is already out. `borrow_mut()` answers that by aborting the
//! process; `AGENTS.md` bans `panic!` on any path that reads SQL text, and a
//! panicking `borrow_mut` is that failure under another name.
//!
//! ## Which callback can actually do it
//!
//! Task-1961's A11 names a scalar function registered through
//! `create_scalar_function`. That one cannot be written: `ScalarBody` is
//! `Arc<dyn Fn(..) + Send + Sync>`, and a `Connection` is neither, so a body has
//! no safe way to hold the connection it was registered on. The bound already
//! prevents the case the finding describes.
//!
//! The authorizer is the one that can. `Connection::set_authorizer` takes an
//! `Rc<dyn Authorizer>` with no `Send` and no `Sync` on it, the binder calls it
//! while the engine borrow is held, and an `Rc<Database>` is exactly what an
//! application would hold. It is the same shape SQLite's
//! `sqlite3_set_authorizer` has, and it reaches the same `borrow_mut` every
//! statement goes through - so proving it here proves it for every other route
//! into the cell.
//!
//! ## And the question that is answered rather than refused
//!
//! A11 made a reentrant call an error; task-1962's A1 step 3 started taking the
//! cases out of it. `sqlite3_get_autocommit` is documented as callable from a
//! callback, and applications do call it there to decide whether they may open
//! a transaction of their own. `Connection::autocommit` reads the writer the
//! database holds beside the engine, so it takes no borrow and answers while
//! the statement that invoked the callback is still running. The last tests
//! below are those answers, and they are values rather than refusals.
//!
//! The run-time limits are the sharper case. `Database::limit` and
//! `Database::set_limit` took `self.engine.borrow()` and `borrow_mut()` with no
//! `try_`, so `sqlite3_limit` asked from inside a callback did not return an
//! error - it aborted the process. They read the settings group the database
//! holds beside the engine now.

use std::cell::RefCell;
use std::rc::Rc;

use inillucent_engine::connect::Database;
use inillucent_engine::{AuthAction, Authorization, Authorizer};

/// A scratch directory of this test's own.
///
/// @param name - what to call it
fn scratch(name: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join("inillucent-reentrant").join(name);
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("the scratch directory is made");
    directory
}

/// An authorizer that runs a query on the connection that is asking it.
struct Reentrant {
    /// The database the query goes to, which is the one being authorized.
    database: Rc<Database>,
    /// What the reentrant query answered, for the assertion below.
    answered: RefCell<Option<String>>,
}

impl Authorizer for Reentrant {
    /// Allows the action, having first tried to query the same connection.
    ///
    /// @param _action - what the binder is asking about
    fn authorize(&self, _action: AuthAction<'_>) -> Authorization {
        if self.answered.borrow().is_none() {
            let outcome = self.database.session().query("SELECT 1");
            // The detail rather than the `Display`, which is the primary
            // code's own sentence and is the same for every misuse.
            let said = match outcome {
                Ok(_) => "the reentrant query ran".to_string(),
                Err(error) => error.detail().unwrap_or("no detail").to_string(),
            };
            *self.answered.borrow_mut() = Some(said);
        }
        Authorization::Allow
    }
}

/// A callback that queries its own connection is refused rather than fatal.
///
/// **The assertion is that this test finishes.** A `borrow_mut` on a cell that
/// is already borrowed aborts the process, and an aborted test binary reports
/// no failure for this case - it reports that the whole target died. So the
/// value asserted is the refusal's own text: the connection said it was busy,
/// which is the answer A11 asks for.
#[test]
fn a_callback_that_queries_its_own_connection_is_refused() {
    let directory = scratch("authorizer");
    let database = Rc::new(Database::open(directory.join("a.rdb")).expect("the database opens"));
    let connection = database.session();
    connection
        .execute("CREATE TABLE t (a INTEGER)")
        .expect("the table is made");

    let watcher = Rc::new(Reentrant {
        database: Rc::clone(&database),
        answered: RefCell::new(None),
    });
    connection
        .set_authorizer(Some(Rc::clone(&watcher) as Rc<dyn Authorizer>))
        .expect("nothing is running on this connection");

    // The statement itself may succeed or be refused; what matters is that the
    // reentrant call inside the authorizer did not take the process with it.
    let _ = connection.query("SELECT a FROM t");

    let said = watcher
        .answered
        .borrow()
        .clone()
        .expect("the authorizer was called at least once");
    assert!(
        said.contains("already running a statement"),
        "a query from inside the authorizer answered {said:?}; it should be \
         refused as a reentrant call, because the engine is one `RefCell` and \
         the statement being authorized is holding it"
    );
}

/// The refusal is `misuse`, which is the code the driver documents for a caller
/// that broke this API's contract.
#[test]
fn the_refusal_is_the_misuse_code() {
    let directory = scratch("code");
    let database = Rc::new(Database::open(directory.join("b.rdb")).expect("the database opens"));
    let connection = database.session();
    connection
        .execute("CREATE TABLE t (a INTEGER)")
        .expect("the table is made");

    let watcher = Rc::new(Reentrant {
        database: Rc::clone(&database),
        answered: RefCell::new(None),
    });
    connection
        .set_authorizer(Some(Rc::clone(&watcher) as Rc<dyn Authorizer>))
        .expect("nothing is running on this connection");
    let _ = connection.query("SELECT a FROM t");

    let said = watcher
        .answered
        .borrow()
        .clone()
        .expect("the authorizer was called at least once");
    assert!(
        said.contains("cannot call back into it"),
        "the refusal said {said:?}, which does not name what the caller did"
    );
}

/// An authorizer that asks whether a transaction is open, from inside one.
struct AsksAutocommit {
    /// The database the question goes to, which is the one being authorized.
    database: Rc<Database>,
    /// Every answer, in the order the authorizer was called.
    answers: RefCell<Vec<Result<bool, String>>>,
}

impl Authorizer for AsksAutocommit {
    /// Allows the action, having first asked the connection for its state.
    ///
    /// @param _action - what the binder is asking about
    fn authorize(&self, _action: AuthAction<'_>) -> Authorization {
        let asked = self
            .database
            .session()
            .autocommit()
            .map_err(|error| error.detail().unwrap_or("no detail").to_string());
        self.answers.borrow_mut().push(asked);
        Authorization::Allow
    }
}

/// A callback asking whether a transaction is open gets `false`, not an error.
///
/// **The one reentrant question that is answered (task-1962, A1 step 3).** The
/// engine's transaction state is ten fields each behind its own cell, held
/// through an `Rc` that `Database` has a second handle on, so this reads it
/// without taking the borrow the running statement holds. Before that it went
/// through `Database::engine`, which is the same cell, and every call from here
/// answered `already running a statement` - for a question SQLite documents as
/// callable from exactly this place.
///
/// The transaction is open, so every answer is `false`. A test that only
/// asserted "not an error" would pass on a stale `true`.
#[test]
fn a_callback_can_ask_whether_a_transaction_is_open() {
    let directory = scratch("autocommit");
    let database = Rc::new(Database::open(directory.join("c.rdb")).expect("the database opens"));
    let connection = database.session();
    connection
        .execute("CREATE TABLE t (a INTEGER)")
        .expect("the table is made");
    connection
        .execute_batch("BEGIN")
        .expect("the transaction opens");

    let watcher = Rc::new(AsksAutocommit {
        database: Rc::clone(&database),
        answers: RefCell::new(Vec::new()),
    });
    connection
        .set_authorizer(Some(Rc::clone(&watcher) as Rc<dyn Authorizer>))
        .expect("nothing is running on this connection");
    let _ = connection.query("SELECT a FROM t");
    connection
        .set_authorizer(None)
        .expect("the authorizer comes off");

    let answers = watcher.answers.borrow().clone();
    assert!(
        !answers.is_empty(),
        "the authorizer was never called, so nothing was asked"
    );
    for answer in &answers {
        assert_eq!(
            answer.as_ref(),
            Ok(&false),
            "a transaction is open, so `autocommit` from inside the authorizer              should answer false; it answered {answer:?}"
        );
    }

    connection
        .execute_batch("ROLLBACK")
        .expect("the transaction closes");
}

/// An authorizer that reads and writes a run-time limit from inside a
/// statement.
struct AsksLimits {
    /// The database the question goes to, which is the one being authorized.
    database: Rc<Database>,
    /// What the limit read as, and what setting it answered, per call.
    answers: RefCell<Vec<(i64, i64)>>,
}

impl Authorizer for AsksLimits {
    /// Allows the action, having first read and set a limit.
    ///
    /// @param _action - what the binder is asking about
    fn authorize(&self, _action: AuthAction<'_>) -> Authorization {
        let read = self
            .database
            .limit(inillucent_base::limits::Limit::VariableNumber);
        let before = self
            .database
            .set_limit(inillucent_base::limits::Limit::VariableNumber, 250);
        self.answers.borrow_mut().push((read, before));
        Authorization::Allow
    }
}

/// A callback can read and set a run-time limit while the statement that called
/// it is running.
///
/// **This one aborted rather than refused (task-1962, A1 step 3).**
/// `Database::limit` and `Database::set_limit` reached the engine through
/// `borrow()` and `borrow_mut()` with no `try_`, and a `RefCell` already
/// borrowed answers those by aborting the process - so an application calling
/// `sqlite3_limit` from an authorizer, which SQLite documents as allowed, took
/// the process with it. The settings are their own group behind their own cells
/// now, and the database holds a handle on it.
///
/// The values are asserted rather than the absence of a crash: the first read
/// is the limit set before the statement started, and the write is read back
/// after it, so a group nothing writes to would fail the second assertion even
/// if it survived the first.
#[test]
fn a_callback_can_read_and_set_a_run_time_limit() {
    let directory = scratch("limits");
    let database = Rc::new(Database::open(directory.join("d.rdb")).expect("the database opens"));
    let connection = database.session();
    connection
        .execute("CREATE TABLE t (a INTEGER)")
        .expect("the table is made");
    database.set_limit(inillucent_base::limits::Limit::VariableNumber, 300);

    let watcher = Rc::new(AsksLimits {
        database: Rc::clone(&database),
        answers: RefCell::new(Vec::new()),
    });
    connection
        .set_authorizer(Some(Rc::clone(&watcher) as Rc<dyn Authorizer>))
        .expect("nothing is running on this connection");
    let _ = connection.query("SELECT a FROM t");
    connection
        .set_authorizer(None)
        .expect("the authorizer comes off");

    let answers = watcher.answers.borrow().clone();
    let (first_read, first_before) = *answers
        .first()
        .expect("the authorizer was called at least once");
    assert_eq!(
        first_read, 300,
        "the first read from inside the authorizer should see the limit set          before the statement started"
    );
    assert_eq!(
        first_before, 300,
        "and setting it should report the same value as the one before it"
    );
    assert_eq!(
        database.limit(inillucent_base::limits::Limit::VariableNumber),
        250,
        "the write the authorizer made should be the one in force afterwards;          a different answer means it wrote to a group nothing else reads"
    );
}

/// An authorizer that asks what the last statement changed.
struct AsksCounters {
    /// The database the question goes to, which is the one being authorized.
    database: Rc<Database>,
    /// The rowid and the change count, per call.
    answers: RefCell<Vec<(i64, i64)>>,
}

impl Authorizer for AsksCounters {
    /// Allows the action, having first read the two counters.
    ///
    /// @param _action - what the binder is asking about
    fn authorize(&self, _action: AuthAction<'_>) -> Authorization {
        let connection = self.database.session();
        let rowid = connection.last_insert_rowid().unwrap_or(-1);
        let changed = connection.changes().unwrap_or(-1);
        self.answers.borrow_mut().push((rowid, changed));
        Authorization::Allow
    }
}

/// A callback can read what the last statement did.
///
/// **`sqlite3_changes` from a hook is the canonical case (task-1962, A1
/// step 3).** An application told that a row changed asks how many, and it asks
/// while the statement that told it is still running. Through
/// `Database::engine` that was the `already running a statement` refusal, so
/// the number came back as an error and the hook had nothing to report.
///
/// The values are asserted: the `INSERT` before the statement assigned rowid 1
/// and changed one row, so a group nothing writes to would answer zero for
/// both.
#[test]
fn a_callback_can_read_what_the_last_statement_did() {
    let directory = scratch("counters");
    let database = Rc::new(Database::open(directory.join("e.rdb")).expect("the database opens"));
    let connection = database.session();
    connection
        .execute("CREATE TABLE t (a INTEGER)")
        .expect("the table is made");
    connection
        .execute("INSERT INTO t VALUES (7)")
        .expect("the row is written");

    let watcher = Rc::new(AsksCounters {
        database: Rc::clone(&database),
        answers: RefCell::new(Vec::new()),
    });
    connection
        .set_authorizer(Some(Rc::clone(&watcher) as Rc<dyn Authorizer>))
        .expect("nothing is running on this connection");
    let _ = connection.query("SELECT a FROM t");
    connection
        .set_authorizer(None)
        .expect("the authorizer comes off");

    let answers = watcher.answers.borrow().clone();
    let (rowid, changed) = *answers
        .first()
        .expect("the authorizer was called at least once");
    assert_eq!(rowid, 1, "the INSERT before the SELECT assigned rowid 1");
    assert_eq!(changed, 1, "and it changed one row");
}
