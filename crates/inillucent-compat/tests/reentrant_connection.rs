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
