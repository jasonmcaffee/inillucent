//! Writing through the public API, and reading it back.
//!
//! Invariant: every assertion here goes through `inillucent_engine::connect::Database`, the
//! same surface an application uses. A test that reached into the pager could
//! pass while the statement layer above it was broken, and the whole point of
//! these is that the layers agree.

use inillucent_compat::workspace_root;
use inillucent_engine::connect::Database;
use inillucent_tree::datum::OwnedDatum;

/// Returns a scratch path nothing else is using.
fn scratch(name: &str) -> std::path::PathBuf {
    let directory = workspace_root().join("_agent_output/dml");
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join(name);
    inillucent_base::testing::remove_database(&path);
    path
}

/// Runs a script and returns the database it ran on.
fn database(name: &str, script: &str) -> (Database, std::path::PathBuf) {
    let path = scratch(name);
    let database = Database::open(&path).expect("opens");
    {
        let connection = database.session();
        connection.execute_batch(script).expect("runs the script");
    }
    (database, path)
}

/// Returns every row a query produces, as owned values.
fn query(database: &Database, sql: &str) -> Vec<Vec<OwnedDatum>> {
    let connection = database.session();
    connection.query(sql).expect("queries")
}

/// Returns one cell as an integer, or `None` when it is not one.
fn integer(rows: &[Vec<OwnedDatum>], row: usize, column: usize) -> Option<i64> {
    match rows.get(row)?.get(column)? {
        OwnedDatum::Int(value) => Some(*value),
        _ => None,
    }
}

/// Returns one cell as text.
fn text(rows: &[Vec<OwnedDatum>], row: usize, column: usize) -> Option<String> {
    match rows.get(row)?.get(column)? {
        OwnedDatum::Text(bytes) => String::from_utf8(bytes.clone()).ok(),
        _ => None,
    }
}

/// Reports whether one cell is NULL.
fn is_null(rows: &[Vec<OwnedDatum>], row: usize, column: usize) -> bool {
    matches!(
        rows.get(row).and_then(|row| row.get(column)),
        Some(OwnedDatum::Null)
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

/// An insert with no rowid, into a table whose largest rowid is `i64::MAX`,
/// finds a free one - and so does the next row of the same statement.
///
/// **It failed `UNIQUE constraint failed` (task-1979, F8).** The allocation was
/// `largest + 1`, saturating, so at `i64::MAX` it answered `i64::MAX` again and
/// the new row collided with the one already holding it. SQLite picks an unused
/// key instead.
///
/// The *second* row is what this test is for, and what the differential corpus
/// does not reach: the high-water mark stays at `i64::MAX` once the counting-up
/// path is exhausted, so each row looks for its own key. A mark that moved to
/// the first free key would make the second row count up from it and collide
/// with whatever sits above it.
#[test]
fn an_insert_past_the_largest_rowid_finds_a_free_one_per_row() {
    let (database, _path) = database(
        "past-max-rowid.db",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
         INSERT INTO t VALUES(9223372036854775807, 'held');
         INSERT INTO t(b) VALUES('first'), ('second');",
    );
    let rows = query(&database, "SELECT count(*), count(DISTINCT a) FROM t");
    assert_eq!(integer(&rows, 0, 0), Some(3), "three rows were inserted");
    assert_eq!(
        integer(&rows, 0, 1),
        Some(3),
        "each row should have taken a key of its own"
    );
    let held = query(&database, "SELECT b FROM t WHERE a = 9223372036854775807");
    assert_eq!(
        held.len(),
        1,
        "the row that held the largest rowid is still there"
    );
}
/// A table whose foreign key points at itself can still be dropped, and a table
/// another table points at still cannot.
///
/// **Both halves are the test (task-1979, F6).** Dropping a table with foreign
/// keys on now runs an implicit `DELETE FROM` first, which is what makes the
/// refusal happen at all - and that delete would refuse a self-referencing
/// table too, because this engine checks an immediate foreign key as each row
/// is written and deleting the first row leaves the second pointing at nothing.
/// SQLite counts those violations to the end of the statement instead, so its
/// count is back to nought once the last row is gone and the drop succeeds. The
/// drop therefore keeps every foreign key trigger but the self-referencing one,
/// and this is the pair that says so: without the second half the first could
/// be passed by dropping the foreign key checks altogether.
///
/// A plain `DELETE FROM` on a self-referencing table is still refused here and
/// still accepted by SQLite. That is the engine's per-row checking and it is a
/// wider change than this ticket; the drop path is what F6 touched.
#[test]
fn a_self_referencing_table_drops_and_a_referenced_one_does_not() {
    let (database, _path) = database(
        "drop-self-reference.db",
        "PRAGMA foreign_keys = ON;
         CREATE TABLE node(id INTEGER PRIMARY KEY, parent REFERENCES node(id));
         INSERT INTO node VALUES(1, NULL), (2, 1);
         CREATE TABLE parent(id INTEGER PRIMARY KEY);
         CREATE TABLE child(pid REFERENCES parent(id));
         INSERT INTO parent VALUES(1);
         INSERT INTO child VALUES(1);",
    );
    {
        let connection = database.session();
        connection
            .execute_batch("PRAGMA foreign_keys = ON; DROP TABLE node;")
            .expect("a table that only references itself can be dropped");
        let refused = connection.execute_batch("PRAGMA foreign_keys = ON; DROP TABLE parent;");
        assert!(
            refused.is_err(),
            "a table a live child row points at cannot be dropped"
        );
    }
    let gone = query(
        &database,
        "SELECT count(*) FROM sqlite_master WHERE name = 'node'",
    );
    assert_eq!(integer(&gone, 0, 0), Some(0), "node was dropped");
    let kept = query(&database, "SELECT count(*) FROM parent");
    assert_eq!(
        integer(&kept, 0, 0),
        Some(1),
        "the refused drop left the parent row where it was"
    );
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
    let connection = database.session();
    connection
        .execute_batch("UPDATE t SET b = 'x' WHERE a >= 2")
        .expect("updates");
    assert_eq!(
        connection
            .changes()
            .expect("nothing is running on this connection"),
        2
    );
    connection
        .execute_batch("DELETE FROM t WHERE a = 1")
        .expect("deletes");
    assert_eq!(
        connection
            .changes()
            .expect("nothing is running on this connection"),
        1
    );
    // The three inserts ran on the script's own connection, and
    // `total_changes` is per connection - so this one has only ever changed
    // the three rows the update and the delete touched.
    assert_eq!(
        connection
            .total_changes()
            .expect("nothing is running on this connection"),
        2 + 1
    );
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
    let connection = database.session();
    connection.execute_batch("BEGIN").expect("begins");
    connection
        .execute_batch("INSERT INTO t VALUES(2, 'gone'); DELETE FROM t WHERE a = 1;")
        .expect("writes");
    assert!(
        !connection
            .autocommit()
            .expect("nothing is running on this connection"),
        "an explicit BEGIN clears autocommit"
    );
    connection.execute_batch("ROLLBACK").expect("rolls back");
    assert!(connection
        .autocommit()
        .expect("nothing is running on this connection"));
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
    let connection = database.session();
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
    let connection = database.session();
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
    let connection = database.session();
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
    let connection = database.session();
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
    let connection = database.session();
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
        let connection = database.session();
        connection
            .execute_batch(
                "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
                 INSERT INTO t VALUES(1, 'one');
                 INSERT INTO t VALUES(2, 'two');",
            )
            .expect("writes");
    }
    let database = Database::open(&path).expect("reopens");
    let connection = database.session();
    let rows = connection
        .query("SELECT a, b FROM t ORDER BY a")
        .expect("queries");
    assert_eq!(rows.len(), 2);
    assert!(
        !std::path::Path::new(&format!("{}-journal", path.display())).exists(),
        "a committed transaction leaves no journal behind"
    );
}

// **The update/commit/rollback hook tests are gone, not rewritten.** The old
// engine's `Connection::set_update_hook`/`set_commit_hook`/`set_rollback_hook`
// have no analog on `inillucent_engine::connect::Connection` -
// `docs/invariants/layering.toml`'s note on the `inillucent` facade names the
// hooks as one of the features the new engine deliberately does not have,
// alongside backup-by-step, blobs and serialize. There is nothing in the new
// engine for a rewritten version of these three cases to assert, so the
// content - "the update hook reports every changed row in order", "a commit
// hook can veto a commit", "the rollback hook fires on an explicit ROLLBACK" -
// is deleted rather than given a hollow replacement. If hooks are added to the
// new engine later, these three cases (and their SQL scripts, preserved above
// in this comment's neighbourhood in source history) are what to restore.

/// **The `CHECK` that failed is the one the refusal names.**
///
/// `source_text_of` looked the constraint up by comparing `declared.name`
/// against the bound constraint's name, and every unnamed `CHECK` has
/// `name == None` - so `None == None` matched the first unnamed constraint
/// whatever had actually failed, and a row violating the second of two was
/// told the first one's text (task-2066 section 4.2, item 27).
///
/// The named arm beside it is the control: names are distinct, so the lookup
/// by name was right for those all along, and a fix that broke them would be
/// trading one wrong answer for another. `differential_part8`'s `t2066-017`
/// and `t2066-018` grade the same two scripts against the reference, but the
/// corpus compares answers rather than refusal text - both engines refuse and
/// that is agreement - so the sentence itself is asserted here.
#[test]
fn the_check_that_failed_is_the_one_named() {
    let (database, _) = database(
        "check-names.rdb",
        "CREATE TABLE unnamed (a INTEGER CHECK (a > 0), b INTEGER CHECK (b > 100))",
    );
    let connection = database.session();
    let refusal = connection
        .execute("INSERT INTO unnamed VALUES (1, 5)")
        .expect_err("the second CHECK must refuse the row");
    let said = refusal.message().to_string();
    assert!(
        said.contains("b > 100"),
        "the refusal did not name the constraint that failed: {said}"
    );
    assert!(
        !said.contains("a > 0"),
        "the refusal named the constraint that passed: {said}"
    );

    connection
        .execute_batch(
            "CREATE TABLE named (a INTEGER, b INTEGER, \
             CONSTRAINT first CHECK (a > 0), CONSTRAINT second CHECK (b > 100))",
        )
        .expect("the named table is created");
    let refusal = connection
        .execute("INSERT INTO named VALUES (1, 5)")
        .expect_err("the second CHECK must refuse the row");
    let said = refusal.message().to_string();
    assert!(
        said.contains("second"),
        "the refusal did not name the constraint that failed: {said}"
    );
    assert!(
        !said.contains("first"),
        "the refusal named the constraint that passed: {said}"
    );
}
