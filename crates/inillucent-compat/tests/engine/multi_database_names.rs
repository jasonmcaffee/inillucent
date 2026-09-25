//! What a name means after the schema set changes under it.
//!
//! Invariant: a qualified name reaches the file it names, whatever has been
//! attached or detached since. The binder numbers schemas by their position -
//! `main`, `temp`, then the attachments in order - so removing one from the
//! middle moves every attachment after it, and an engine that did not move them
//! with it would answer `two.t` out of the file `three.t` is in. That is a wrong
//! answer rather than a refusal, which is why it is a test rather than a
//! comment.
//!
//! `attach.rs` grades resolution against SQLite with one attachment. This grades
//! the renumbering, which needs three.

use std::path::PathBuf;

use inillucent_compat::facade::Database;
use inillucent_compat::workspace_root;
use inillucent_value::Value;

/// Returns a scratch directory for one scenario, emptied first.
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output/names").join(name);
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::create_dir_all(&directory);
    directory
}

/// Returns a path as a statement writes it.
///
/// @param path - the file
fn quoted(path: &std::path::Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// Returns the one integer a query answers with.
///
/// @param connection - the connection to ask
/// @param sql - the query
fn count(connection: &inillucent_compat::facade::Connection, sql: &str) -> Option<i64> {
    connection
        .query(sql)
        .ok()?
        .first()
        .and_then(|row| row.first())
        .and_then(Value::as_integer)
}

/// Detaching one of three attachments leaves the other two reachable by name.
#[test]
fn detaching_the_middle_database_does_not_move_the_others() {
    let directory = scratch("renumber");
    let main = directory.join("main.db");
    let database = Database::open(&main).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    // Three attachments, each with a table holding a different number of rows,
    // so that reading the wrong file is visible rather than merely possible.
    for (name, rows) in [("one", 1), ("two", 2), ("three", 3)] {
        let file = directory.join(format!("{name}.db"));
        connection
            .execute_batch(&format!(
                "ATTACH DATABASE '{}' AS {name}; CREATE TABLE {name}.t(a);",
                quoted(&file)
            ))
            .expect("the database attaches");
        for value in 0..rows {
            connection
                .execute_batch(&format!("INSERT INTO {name}.t VALUES ({value})"))
                .expect("the row is written");
        }
    }
    assert_eq!(count(&connection, "SELECT count(*) FROM one.t"), Some(1));
    assert_eq!(count(&connection, "SELECT count(*) FROM two.t"), Some(2));
    assert_eq!(count(&connection, "SELECT count(*) FROM three.t"), Some(3));

    connection
        .execute_batch("DETACH DATABASE two")
        .expect("the middle database detaches");

    assert!(
        connection.query("SELECT count(*) FROM two.t").is_err(),
        "a detached database is still answering"
    );
    assert_eq!(
        count(&connection, "SELECT count(*) FROM one.t"),
        Some(1),
        "the attachment before the detached one moved"
    );
    assert_eq!(
        count(&connection, "SELECT count(*) FROM three.t"),
        Some(3),
        "the attachment after the detached one is reading another file"
    );
    // And it is still writable, which is the half a read cannot see: a write
    // goes through the schema number as well.
    connection
        .execute_batch("INSERT INTO three.t VALUES (99)")
        .expect("the attachment after the detached one is still writable");
    assert_eq!(count(&connection, "SELECT count(*) FROM three.t"), Some(4));
    assert_eq!(count(&connection, "SELECT count(*) FROM one.t"), Some(1));
}

/// A name freed by `DETACH` can be taken again, by a different file.
#[test]
fn a_detached_name_can_be_reused_for_another_file() {
    let directory = scratch("reuse");
    let main = directory.join("main.db");
    let first = directory.join("first.db");
    let second = directory.join("second.db");
    let database = Database::open(&main).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    connection
        .execute_batch(&format!(
            "ATTACH DATABASE '{}' AS aux;
             CREATE TABLE aux.t(a);
             INSERT INTO aux.t VALUES (1);
             DETACH DATABASE aux;
             ATTACH DATABASE '{}' AS aux;
             CREATE TABLE aux.t(a);
             INSERT INTO aux.t VALUES (1),(2);",
            quoted(&first),
            quoted(&second)
        ))
        .expect("the name is taken twice");
    assert_eq!(
        count(&connection, "SELECT count(*) FROM aux.t"),
        Some(2),
        "the name is still bound to the file it was detached from"
    );
}
