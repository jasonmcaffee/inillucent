//! Preparing, binding and stepping a statement through the new engine.
//!
//! Invariant: **a statement compiled once answers each new binding, and reading
//! a row you were not given is not possible.** This is the shape
//! `inillucent-migrate` uses - `prepare`, `bind_*`, `step`, `row` - and it is
//! the first piece of the connection surface added because a caller being moved
//! reached for it, rather than because the old connection had it.
//!
//! ## It materialises, and that is written down rather than discovered
//!
//! `step` runs the whole statement on its first call and then walks the rows it
//! produced, where SQLite's `sqlite3_step` produces one row at a time. To a
//! caller the shape is identical; the memory is not. The engine's executor is
//! batch-at-a-time and its sinks collect, so a row-at-a-time `step` would be a
//! different executor rather than a different wrapper. The tests below are
//! written to the materialising behaviour deliberately, so that the day it
//! changes they say so.

use inillucent_compat::workspace_root;
use inillucent_engine::connect::Database;
use inillucent_tree::datum::OwnedDatum;

/// Returns a database holding a small table.
///
/// @param name - the test's name, which names its file
fn fixture(name: &str) -> Database {
    let area = workspace_root().join("target/scratch/statement");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join(format!("{name}.rdb"));
    let _ = std::fs::remove_file(&path);
    let database = Database::open(&path).expect("a fresh database opens");
    {
        let connection = database.session();
        connection
            .execute_batch(
                "CREATE TABLE item (id INTEGER PRIMARY KEY, name TEXT, weight INTEGER); \
                 INSERT INTO item(id, name, weight) VALUES (1, 'anvil', 100); \
                 INSERT INTO item(id, name, weight) VALUES (2, 'bell', 250); \
                 INSERT INTO item(id, name, weight) VALUES (3, 'cog', 250)",
            )
            .expect("the fixture loads");
    }
    database
}

/// Returns a row's first column as text.
///
/// @param row - the row
fn text(row: &[OwnedDatum]) -> String {
    match row.first() {
        Some(OwnedDatum::Text(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
        other => panic!("expected text, found {other:?}"),
    }
}

/// A prepared statement steps once per row and then stops.
#[test]
fn a_statement_steps_once_per_row() {
    let database = fixture("steps");
    let connection = database.session();
    let mut statement = connection
        .prepare("SELECT name FROM item ORDER BY id")
        .expect("the statement compiles");

    let mut names = Vec::new();
    while statement.step().expect("stepping works") {
        names.push(text(statement.row()));
    }
    assert_eq!(
        names,
        vec!["anvil".to_string(), "bell".to_string(), "cog".to_string()]
    );
    assert!(
        !statement
            .step()
            .expect("stepping past the end is not an error"),
        "a statement that ran out of rows kept producing them"
    );
}

/// A row is empty before the first step, rather than the first row.
///
/// A caller that ignored `step`'s answer and read `row` would otherwise be
/// handed a row it was never given - which is the kind of thing that reads
/// correctly in a test and is wrong in a loop with an early exit.
#[test]
fn a_row_before_the_first_step_is_empty() {
    let database = fixture("unstepped");
    let connection = database.session();
    let statement = connection
        .prepare("SELECT name FROM item ORDER BY id")
        .expect("the statement compiles");
    assert!(
        statement.row().is_empty(),
        "a statement that has not stepped handed out a row"
    );
}

/// The same statement answers each new binding.
#[test]
fn a_statement_answers_each_new_binding() {
    let database = fixture("rebind");
    let connection = database.session();
    let mut statement = connection
        .prepare("SELECT name FROM item WHERE id = ?1")
        .expect("the statement compiles");

    for (id, expected) in [(1, "anvil"), (3, "cog"), (2, "bell")] {
        statement.bind_integer(1, id).expect("the binding takes");
        assert!(
            statement.step().expect("stepping works"),
            "id {id} is there"
        );
        assert_eq!(text(statement.row()), expected);
    }

    // A binding that matches nothing produces no row rather than the last one.
    statement.bind_integer(1, 99).expect("the binding takes");
    assert!(
        !statement.step().expect("stepping works"),
        "a binding that matches nothing produced a row"
    );
    assert!(statement.row().is_empty());
}

/// Every binding kind reaches the value it stands for.
#[test]
fn each_binding_kind_reaches_the_value() {
    let database = fixture("kinds");
    let connection = database.session();
    connection
        .execute_batch("CREATE TABLE held (id INTEGER PRIMARY KEY, a TEXT, b BLOB, c INTEGER)")
        .expect("the table is created");

    let mut insert = connection
        .prepare("INSERT INTO held(id, a, b, c) VALUES (?1, ?2, ?3, ?4)")
        .expect("the insert compiles");
    insert.bind_integer(1, 7).expect("bound");
    insert.bind_text(2, "seven").expect("bound");
    insert.bind_blob(3, &[0xde, 0xad]).expect("bound");
    insert.bind_null(4).expect("bound");
    assert!(
        !insert.step().expect("the insert runs"),
        "an insert has no rows"
    );
    assert_eq!(insert.changes(), 1, "one row was written");

    let rows = connection
        .query("SELECT a, b, c FROM held WHERE id = 7")
        .expect("the row reads back");
    let row = rows.first().expect("the row is there");
    assert_eq!(
        row.first(),
        Some(&OwnedDatum::Text(b"seven".to_vec())),
        "text did not survive the binding"
    );
    assert_eq!(
        row.get(1),
        Some(&OwnedDatum::Blob(vec![0xde, 0xad])),
        "a blob did not survive the binding"
    );
    assert_eq!(row.get(2), Some(&OwnedDatum::Null), "NULL did not survive");
}

/// Binding a high parameter first leaves the lower ones NULL.
///
/// The alternative - growing the list by pushing - would make `?3` bound before
/// `?1` land in slot 1, which is a wrong answer rather than a missing one.
#[test]
fn binding_out_of_order_does_not_shift_the_parameters() {
    let database = fixture("outoforder");
    let connection = database.session();
    connection
        .execute_batch("CREATE TABLE pair (id INTEGER PRIMARY KEY, a TEXT, b TEXT)")
        .expect("the table is created");

    let mut insert = connection
        .prepare("INSERT INTO pair(id, a, b) VALUES (1, ?1, ?2)")
        .expect("the insert compiles");
    insert.bind_text(2, "second").expect("bound");
    assert!(!insert.step().expect("the insert runs"));

    let rows = connection
        .query("SELECT a, b FROM pair")
        .expect("read back");
    let row = rows.first().expect("the row is there");
    assert_eq!(row.first(), Some(&OwnedDatum::Null), "?1 was never bound");
    assert_eq!(row.get(1), Some(&OwnedDatum::Text(b"second".to_vec())));
}

/// The connection reports what the last statement changed.
#[test]
fn the_connection_reports_the_last_statement_s_changes() {
    let database = fixture("changes");
    let connection = database.session();

    connection
        .execute_batch("UPDATE item SET weight = 1 WHERE weight = 250")
        .expect("the update runs");
    assert_eq!(
        connection
            .changes()
            .expect("nothing is running on this connection"),
        2,
        "two rows had weight 250"
    );

    connection
        .execute_batch("DELETE FROM item WHERE id = 1")
        .expect("the delete runs");
    assert_eq!(
        connection
            .changes()
            .expect("nothing is running on this connection"),
        1
    );
}
