//! Writing through the public API, and reading it back.
//!
//! Invariant: every assertion here goes through `inillucent_legacy::Database`, the same
//! surface an application uses. A test that reached into the pager could pass
//! while the statement layer above it was broken, and the whole point of these
//! is that the layers agree.

use inillucent_compat::workspace_root;
use inillucent_legacy::{Database, Value};

/// Returns a scratch path nothing else is using.
fn scratch(name: &str) -> std::path::PathBuf {
    let directory = workspace_root().join("_agent_output/dml");
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join(name);
    for suffix in ["", "-journal"] {
        let candidate = directory.join(format!("{name}{suffix}"));
        let _ = std::fs::remove_file(candidate);
    }
    path
}

/// Runs a script and returns the connection it ran on.
fn database(name: &str, script: &str) -> (Database, std::path::PathBuf) {
    let path = scratch(name);
    let database = Database::open(&path).expect("opens");
    {
        let connection = database.connect().expect("connects");
        connection.execute_batch(script).expect("runs the script");
    }
    (database, path)
}

/// Returns every row a query produces, as owned values.
fn query(database: &Database, sql: &str) -> Vec<Vec<Value<'static>>> {
    let connection = database.connect().expect("connects");
    connection.query(sql).expect("queries")
}

/// Returns one cell as an integer, or `None` when it is not one.
fn integer(rows: &[Vec<Value<'static>>], row: usize, column: usize) -> Option<i64> {
    rows.get(row)?.get(column)?.as_integer()
}

/// Returns one cell as text.
fn text(rows: &[Vec<Value<'static>>], row: usize, column: usize) -> Option<String> {
    let value = rows.get(row)?.get(column)?;
    let bytes = value.as_text()?.utf8_bytes().into_owned();
    String::from_utf8(bytes).ok()
}

/// Reports whether one cell is NULL.
fn is_null(rows: &[Vec<Value<'static>>], row: usize, column: usize) -> bool {
    matches!(
        rows.get(row).and_then(|row| row.get(column)),
        Some(Value::Null)
    )
}

/// A table can be created, written, and read back.
#[test]
fn a_table_round_trips_through_the_public_api() {
    let (database, _path) = database(
        "round-trip.db",
        "CREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT, score REAL);
         INSERT INTO people VALUES(1, 'ada', 9.5);
         INSERT INTO people(name, score) VALUES('grace', 8.25);
         INSERT INTO people VALUES(7, 'alan', NULL);",
    );
    let rows = query(&database, "SELECT id, name, score FROM people ORDER BY id");
    assert_eq!(rows.len(), 3);
    assert_eq!(integer(&rows, 0, 0), Some(1));
    assert_eq!(
        integer(&rows, 1, 0),
        Some(2),
        "the second row should have taken the next rowid"
    );
    assert_eq!(integer(&rows, 2, 0), Some(7));
    assert!(is_null(&rows, 2, 2));
}

/// The change counters report what a statement did.
#[test]
fn the_counters_report_what_a_statement_changed() {
    let (database, _path) = database(
        "counters.db",
        "CREATE TABLE t(a INTEGER, b TEXT);
         INSERT INTO t VALUES(1, 'one');
         INSERT INTO t VALUES(2, 'two');
         INSERT INTO t VALUES(3, 'three');",
    );
    let connection = database.connect().expect("connects");
    connection
        .execute_batch("UPDATE t SET b = 'x' WHERE a >= 2")
        .expect("updates");
    assert_eq!(connection.changes(), 2);
    connection
        .execute_batch("DELETE FROM t WHERE a = 1")
        .expect("deletes");
    assert_eq!(connection.changes(), 1);
    // The three inserts ran on the script's own connection, and
    // `total_changes` is per connection - so this one has only ever changed
    // the three rows the update and the delete touched.
    assert_eq!(connection.total_changes(), 2 + 1);
    let rows = connection.query("SELECT count(*) FROM t").expect("counts");
    assert_eq!(integer(&rows, 0, 0), Some(2));
}

/// A rolled back transaction leaves nothing behind.
#[test]
fn a_rolled_back_transaction_changes_nothing() {
    let (database, _path) = database(
        "rollback.db",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
         INSERT INTO t VALUES(1, 'kept');",
    );
    let connection = database.connect().expect("connects");
    connection.execute_batch("BEGIN").expect("begins");
    connection
        .execute_batch("INSERT INTO t VALUES(2, 'gone'); DELETE FROM t WHERE a = 1;")
        .expect("writes");
    assert!(
        !connection.autocommit(),
        "an explicit BEGIN clears autocommit"
    );
    connection.execute_batch("ROLLBACK").expect("rolls back");
    assert!(connection.autocommit());
    let rows = connection.query("SELECT a, b FROM t").expect("queries");
    assert_eq!(rows.len(), 1);
    assert_eq!(integer(&rows, 0, 0), Some(1));
}

/// A savepoint undoes only what it covers.
#[test]
fn a_savepoint_undoes_only_what_it_covers() {
    let (database, _path) = database(
        "savepoint.db",
        "CREATE TABLE t(a INTEGER PRIMARY KEY);
         INSERT INTO t VALUES(1);",
    );
    let connection = database.connect().expect("connects");
    connection
        .execute_batch(
            "BEGIN;
             INSERT INTO t VALUES(2);
             SAVEPOINT s;
             INSERT INTO t VALUES(3);
             ROLLBACK TO s;
             COMMIT;",
        )
        .expect("runs");
    let rows = connection
        .query("SELECT a FROM t ORDER BY a")
        .expect("queries");
    assert_eq!(rows.len(), 2, "the row inside the savepoint should be gone");
}

/// A UNIQUE constraint refuses a duplicate, and IGNORE skips it.
#[test]
fn a_unique_constraint_refuses_a_duplicate() {
    let (database, _path) = database(
        "unique.db",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT UNIQUE);
         INSERT INTO t VALUES(1, 'one');",
    );
    let connection = database.connect().expect("connects");
    let failure = connection
        .execute_batch("INSERT INTO t VALUES(2, 'one')")
        .expect_err("the duplicate is refused");
    assert_eq!(failure.extended().value(), 2067, "SQLITE_CONSTRAINT_UNIQUE");

    connection
        .execute_batch("INSERT OR IGNORE INTO t VALUES(2, 'one')")
        .expect("ignored");
    let rows = connection.query("SELECT count(*) FROM t").expect("counts");
    assert_eq!(integer(&rows, 0, 0), Some(1));

    connection
        .execute_batch("INSERT OR REPLACE INTO t VALUES(3, 'one')")
        .expect("replaced");
    let rows = connection.query("SELECT a, b FROM t").expect("queries");
    assert_eq!(rows.len(), 1);
    assert_eq!(integer(&rows, 0, 0), Some(3));
}

/// NOT NULL and CHECK are enforced.
#[test]
fn not_null_and_check_are_enforced() {
    let (database, _path) = database(
        "constraints.db",
        "CREATE TABLE t(a INTEGER NOT NULL, b INTEGER CHECK (b > 0));",
    );
    let connection = database.connect().expect("connects");
    let failure = connection
        .execute_batch("INSERT INTO t VALUES(NULL, 1)")
        .expect_err("NOT NULL refuses");
    assert_eq!(
        failure.extended().value(),
        1299,
        "SQLITE_CONSTRAINT_NOTNULL, got {} / {}",
        failure.extended().value(),
        failure.message()
    );
    let failure = connection
        .execute_batch("INSERT INTO t VALUES(1, 0)")
        .expect_err("CHECK refuses");
    assert_eq!(failure.extended().value(), 275, "SQLITE_CONSTRAINT_CHECK");
    connection
        .execute_batch("INSERT INTO t VALUES(1, NULL)")
        .expect("a NULL is not a CHECK violation");
    let rows = connection.query("SELECT count(*) FROM t").expect("counts");
    assert_eq!(integer(&rows, 0, 0), Some(1));
}

/// An index is created, backfilled, and used by a later query.
#[test]
fn an_index_is_backfilled_and_maintained() {
    let (database, _path) = database(
        "index.db",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
         INSERT INTO t VALUES(1, 'aaa');
         INSERT INTO t VALUES(2, 'bbb');
         INSERT INTO t VALUES(3, 'ccc');
         CREATE INDEX t_b ON t(b);
         INSERT INTO t VALUES(4, 'ddd');",
    );
    let rows = query(&database, "SELECT a FROM t WHERE b = 'bbb'");
    assert_eq!(integer(&rows, 0, 0), Some(2));
    let rows = query(&database, "SELECT a FROM t WHERE b = 'ddd'");
    assert_eq!(integer(&rows, 0, 0), Some(4));
    let rows = query(&database, "SELECT count(*) FROM t WHERE b > 'aaa'");
    assert_eq!(integer(&rows, 0, 0), Some(3));
}

/// A dropped table takes its rows and its schema entry with it.
#[test]
fn a_dropped_table_is_gone() {
    let (database, _path) = database(
        "drop.db",
        "CREATE TABLE keep(a INTEGER);
         CREATE TABLE go(a INTEGER, b TEXT UNIQUE);
         INSERT INTO go VALUES(1, 'x');
         INSERT INTO keep VALUES(1);
         DROP TABLE go;",
    );
    let connection = database.connect().expect("connects");
    assert!(connection.query("SELECT * FROM go").is_err());
    let rows = connection
        .query("SELECT count(*) FROM keep")
        .expect("counts");
    assert_eq!(integer(&rows, 0, 0), Some(1));
}

/// RETURNING reports the row that was written.
#[test]
fn returning_reports_the_written_row() {
    let (database, _path) = database(
        "returning.db",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);",
    );
    let connection = database.connect().expect("connects");
    let rows = connection
        .query("INSERT INTO t(b) VALUES('one') RETURNING a, b")
        .expect("inserts");
    assert_eq!(rows.len(), 1);
    assert_eq!(integer(&rows, 0, 0), Some(1));
    let rows = connection
        .query("UPDATE t SET b = 'two' RETURNING a, b")
        .expect("updates");
    assert_eq!(text(&rows, 0, 1), Some("two".to_string()));
    let rows = connection
        .query("DELETE FROM t RETURNING a")
        .expect("deletes");
    assert_eq!(rows.len(), 1);
    let rows = connection.query("SELECT count(*) FROM t").expect("counts");
    assert_eq!(integer(&rows, 0, 0), Some(0));
}

/// Everything written survives closing and reopening the file.
#[test]
fn writes_survive_a_close_and_reopen() {
    let path = scratch("reopen.db");
    {
        let database = Database::open(&path).expect("opens");
        let connection = database.connect().expect("connects");
        connection
            .execute_batch(
                "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
                 INSERT INTO t VALUES(1, 'one');
                 INSERT INTO t VALUES(2, 'two');",
            )
            .expect("writes");
    }
    let database = Database::open(&path).expect("reopens");
    let connection = database.connect().expect("connects");
    let rows = connection
        .query("SELECT a, b FROM t ORDER BY a")
        .expect("queries");
    assert_eq!(rows.len(), 2);
    assert!(
        !std::path::Path::new(&format!("{}-journal", path.display())).exists(),
        "a committed transaction leaves no journal behind"
    );
}

/// The update hook reports every row a statement changed, in order.
#[test]
fn the_update_hook_reports_every_changed_row() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let (database, _path) = database(
        "hooks.db",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
         INSERT INTO t VALUES(1, 'one');
         INSERT INTO t VALUES(2, 'two');",
    );
    let connection = database.connect().expect("connects");
    let seen = Rc::new(RefCell::new(Vec::new()));
    let recorder = Rc::clone(&seen);
    connection.set_update_hook(Some(Box::new(move |kind, database, table, rowid| {
        recorder.borrow_mut().push(format!(
            "{} {}.{} {rowid}",
            kind.as_str(),
            String::from_utf8_lossy(database),
            String::from_utf8_lossy(table)
        ));
    })));

    connection
        .execute_batch("INSERT INTO t VALUES(3, 'three')")
        .expect("inserts");
    connection
        .execute_batch("UPDATE t SET b = 'x' WHERE a >= 2")
        .expect("updates");
    connection
        .execute_batch("DELETE FROM t WHERE a = 1")
        .expect("deletes");

    assert_eq!(
        seen.borrow().as_slice(),
        [
            "INSERT main.t 3",
            "UPDATE main.t 2",
            "UPDATE main.t 3",
            "DELETE main.t 1",
        ]
    );

    // Removing the hook stops the reports.
    connection.set_update_hook(None);
    connection
        .execute_batch("INSERT INTO t VALUES(4, 'four')")
        .expect("inserts");
    assert_eq!(seen.borrow().len(), 4, "a removed hook still fired");
}

/// A commit hook can veto a commit, which becomes a rollback.
#[test]
fn a_commit_hook_can_veto_a_commit() {
    use std::cell::Cell;
    use std::rc::Rc;

    let (database, _path) = database(
        "commit-hook.db",
        "CREATE TABLE t(a INTEGER PRIMARY KEY);
         INSERT INTO t VALUES(1);",
    );
    let connection = database.connect().expect("connects");
    let veto = Rc::new(Cell::new(false));
    let rolled_back = Rc::new(Cell::new(0u32));
    let asked = Rc::clone(&veto);
    connection.set_commit_hook(Some(Box::new(move || asked.get())));
    let counted = Rc::clone(&rolled_back);
    connection.set_rollback_hook(Some(Box::new(move || {
        counted.set(counted.get().saturating_add(1));
    })));

    // With the hook allowing it, the write lands.
    connection
        .execute_batch("INSERT INTO t VALUES(2)")
        .expect("inserts");
    assert_eq!(
        connection.query("SELECT a FROM t").expect("queries").len(),
        2
    );
    assert_eq!(rolled_back.get(), 0);

    // With the hook vetoing, the write does not - and the veto is reported as
    // a rollback rather than as an error.
    veto.set(true);
    connection
        .execute_batch("INSERT INTO t VALUES(3)")
        .expect("the veto is not an error");
    assert_eq!(
        connection.query("SELECT a FROM t").expect("queries").len(),
        2,
        "the vetoed row was written anyway"
    );
    assert_eq!(rolled_back.get(), 1, "the rollback hook did not fire");

    // An explicit transaction is vetoed the same way.
    connection.execute_batch("BEGIN").expect("begins");
    connection
        .execute_batch("INSERT INTO t VALUES(4)")
        .expect("inserts");
    connection
        .execute_batch("COMMIT")
        .expect("the veto is not an error");
    assert_eq!(
        connection.query("SELECT a FROM t").expect("queries").len(),
        2
    );
    assert_eq!(rolled_back.get(), 2);
}

/// The rollback hook fires for an explicit ROLLBACK too.
#[test]
fn the_rollback_hook_fires_on_an_explicit_rollback() {
    use std::cell::Cell;
    use std::rc::Rc;

    let (database, _path) = database("rollback-hook.db", "CREATE TABLE t(a INTEGER PRIMARY KEY);");
    let connection = database.connect().expect("connects");
    let fired = Rc::new(Cell::new(0u32));
    let counted = Rc::clone(&fired);
    connection.set_rollback_hook(Some(Box::new(move || {
        counted.set(counted.get().saturating_add(1));
    })));

    connection
        .execute_batch("BEGIN; INSERT INTO t VALUES(1); ROLLBACK;")
        .expect("runs");
    assert_eq!(fired.get(), 1);
    connection
        .execute_batch("INSERT INTO t VALUES(2)")
        .expect("inserts");
    assert_eq!(
        fired.get(),
        1,
        "a successful commit fired the rollback hook"
    );
}
