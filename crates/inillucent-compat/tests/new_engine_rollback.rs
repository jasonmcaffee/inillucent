//! Abandoning a transaction: `ROLLBACK`, `SAVEPOINT` and `RELEASE`.
//!
//! Invariant: **a transaction that is abandoned leaves the database as it was
//! before it opened - its rows and its schema both.** `BEGIN` used to map to
//! the engine's log batching, which groups writes so the log is flushed once;
//! that is a durability grouping and not atomicity, and it left a caller able
//! to enter a transaction and unable to leave one. `inillucent-cli`'s import
//! path is such a caller, and so is `inillucent-migrate`'s.
//!
//! ## What the undo is, and what it is not
//!
//! The write-ahead log is **redo-only**: `InsertRow`, `DeleteRow` and
//! `UpdateInPlace` carry an after-image and no before-image, so a rollback
//! cannot be a backwards replay of it. The before-images are collected
//! separately, in memory, while a transaction is open - and only while one is
//! open, so an autocommit statement pays nothing for a possibility it does not
//! have.
//!
//! The restores are themselves ordinary logged writes. They have to be: the log
//! is redo-only, so a crash between the rollback and the commit has to replay
//! to the *rolled back* state and not to the state the abandoned statements
//! left behind.
//!
//! ## The schema is undone the same way
//!
//! A catalog row is a row. A `CREATE TABLE` inside a transaction writes one,
//! and abandoning the transaction takes it out again - after which the
//! in-memory schema is rebuilt from the catalog tree, because that tree is the
//! authority and the list the binder reads is a cache of it.

use inillucent_compat::workspace_root;
use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

/// Returns a database holding three rows.
///
/// @param name - the test's name, which names its file
fn fixture(name: &str) -> Database {
    let area = workspace_root().join("target/scratch/rollback");
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

/// Returns how many rows a table holds, or `None` when there is no such table.
///
/// @param connection - the connection to ask
/// @param table - the table's name
fn rows(connection: &Connection<'_>, table: &str) -> Option<i64> {
    let answer = connection
        .query(&format!("SELECT count(*) FROM {table}"))
        .ok()?;
    match answer.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Int(number)) => Some(*number),
        _ => None,
    }
}

/// Returns one row's name column.
///
/// @param connection - the connection to ask
/// @param id - the row's key
fn name_of(connection: &Connection<'_>, id: i64) -> Option<String> {
    let answer = connection
        .query(&format!("SELECT name FROM item WHERE id = {id}"))
        .ok()?;
    match answer.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Text(bytes)) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

/// Every kind of change a transaction can make is undone by abandoning it.
///
/// The three are tested together rather than apart because they undo
/// differently - an insert is undone by a delete, a delete by writing the row
/// back, an update by writing the *whole* row back rather than the column that
/// changed - and a rollback that got one of them right and another wrong would
/// pass a test that only made one kind of change.
#[test]
fn an_abandoned_transaction_undoes_every_kind_of_change() {
    let database = fixture("every-kind");
    let connection = database.session();
    assert_eq!(rows(&connection, "item"), Some(3));

    connection.execute_batch("BEGIN").expect("the batch opens");
    connection
        .execute_batch(
            "INSERT INTO item(id, name, weight) VALUES (4, 'drum', 50); \
             INSERT INTO item(id, name, weight) VALUES (5, 'edge', 10); \
             UPDATE item SET name = 'changed' WHERE id = 1; \
             DELETE FROM item WHERE id = 2",
        )
        .expect("the statements apply");
    assert_eq!(rows(&connection, "item"), Some(4), "3 + 2 - 1");
    assert_eq!(name_of(&connection, 1).as_deref(), Some("changed"));

    connection
        .query("ROLLBACK")
        .expect("the transaction is abandoned");

    assert_eq!(rows(&connection, "item"), Some(3), "the inserts went away");
    assert_eq!(
        name_of(&connection, 1).as_deref(),
        Some("anvil"),
        "the update was not undone"
    );
    assert_eq!(
        name_of(&connection, 2).as_deref(),
        Some("bell"),
        "the deleted row did not come back"
    );
    database.check().expect("every tree is intact");
}

/// A committed transaction keeps what it did.
///
/// The other half of the same claim: an undo buffer that rolled back on commit
/// would pass every test above.
#[test]
fn a_committed_transaction_keeps_what_it_did() {
    let database = fixture("committed");
    let connection = database.session();

    connection.execute_batch("BEGIN").expect("the batch opens");
    connection
        .execute_batch("INSERT INTO item(id, name, weight) VALUES (4, 'drum', 50)")
        .expect("the insert applies");
    connection.execute_batch("COMMIT").expect("it commits");

    assert_eq!(rows(&connection, "item"), Some(4));
    database.check().expect("every tree is intact");
}

/// A savepoint is undone back to without abandoning the transaction.
#[test]
fn a_savepoint_is_a_point_the_transaction_returns_to() {
    let database = fixture("savepoint");
    let connection = database.session();

    connection.execute_batch("BEGIN").expect("the batch opens");
    connection
        .execute_batch("INSERT INTO item(id, name, weight) VALUES (4, 'drum', 50)")
        .expect("kept");
    connection
        .execute_batch("SAVEPOINT half")
        .expect("the savepoint is named");
    connection
        .execute_batch(
            "INSERT INTO item(id, name, weight) VALUES (5, 'edge', 10); \
             INSERT INTO item(id, name, weight) VALUES (6, 'flute', 20)",
        )
        .expect("discarded");
    assert_eq!(rows(&connection, "item"), Some(6));

    connection
        .query("ROLLBACK TO half")
        .expect("the savepoint is returned to");
    assert_eq!(
        rows(&connection, "item"),
        Some(4),
        "the two after the savepoint went away and the one before it did not"
    );

    connection
        .query("RELEASE half")
        .expect("the savepoint is forgotten");
    connection.execute_batch("COMMIT").expect("it commits");
    assert_eq!(rows(&connection, "item"), Some(4));
    database.check().expect("every tree is intact");
}

/// A savepoint that was never named is refused rather than ignored.
#[test]
fn an_unnamed_savepoint_is_refused() {
    let database = fixture("unnamed");
    let connection = database.session();
    connection.execute_batch("BEGIN").expect("the batch opens");

    let error = connection
        .query("ROLLBACK TO nowhere")
        .expect_err("a savepoint nobody named is not a savepoint");
    assert!(
        format!("{error:?}").contains("no such savepoint"),
        "the refusal did not name what it refused: {error:?}"
    );
}

/// A table created inside an abandoned transaction is not in the schema after.
///
/// The catalog row is undone like any other row, and then the schema the binder
/// reads is rebuilt from the catalog tree. Checking `sqlite_schema` as well as
/// the table itself is deliberate: a rollback that removed the row from the
/// tree and left it in the cache would answer "no such table" for a query and
/// still list it.
#[test]
fn a_table_created_in_an_abandoned_transaction_is_gone() {
    let database = fixture("ddl");
    let connection = database.session();

    connection.execute_batch("BEGIN").expect("the batch opens");
    connection
        .execute_batch("CREATE TABLE made (id INTEGER PRIMARY KEY, a TEXT)")
        .expect("the table is created");
    assert_eq!(rows(&connection, "made"), Some(0), "it exists inside");

    connection
        .query("ROLLBACK")
        .expect("the transaction is abandoned");

    assert_eq!(
        rows(&connection, "made"),
        None,
        "it is not a table any more"
    );
    let named: Vec<String> = connection
        .query("SELECT name FROM sqlite_schema")
        .expect("the schema is queryable")
        .iter()
        .filter_map(|row| match row.first() {
            Some(OwnedDatum::Text(bytes)) => Some(String::from_utf8_lossy(bytes).into_owned()),
            _ => None,
        })
        .collect();
    assert!(
        !named.iter().any(|held| held == "made"),
        "the schema still lists it: {named:?}"
    );
    database.check().expect("every tree is intact");
}

/// What was rolled back stays rolled back after a reopen.
///
/// The restores are logged, so recovery replays to the state the rollback left
/// rather than to the one the abandoned statements did. A rollback that undid
/// only the pages would pass every test above and fail this one.
#[test]
fn a_rollback_survives_the_next_open() {
    let area = workspace_root().join("target/scratch/rollback");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join("reopened.rdb");
    let _ = std::fs::remove_file(&path);
    {
        let database = Database::open(&path).expect("created");
        let connection = database.session();
        connection
            .execute_batch(
                "CREATE TABLE item (id INTEGER PRIMARY KEY, name TEXT); \
                 INSERT INTO item(id, name) VALUES (1, 'anvil')",
            )
            .expect("the fixture loads");
        connection.execute_batch("BEGIN").expect("the batch opens");
        for id in 2..40 {
            connection
                .execute_batch(&format!(
                    "INSERT INTO item(id, name) VALUES ({id}, 'row {id}')"
                ))
                .expect("the insert applies");
        }
        connection.query("ROLLBACK").expect("abandoned");
        database.checkpoint().expect("checkpointed");
    }

    let database = Database::open(&path).expect("opened");
    let connection = database.session();
    assert_eq!(
        rows(&connection, "item"),
        Some(1),
        "the abandoned inserts came back after a reopen"
    );
    database.check().expect("every tree is intact");
}
