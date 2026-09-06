//! Functions and collations an application defines, compared against SQLite.
//!
//! Invariant: a registered function behaves like a built-in everywhere a
//! built-in works. The interesting cases are not "does it get called" but the
//! ones where a function's *kind* matters: an aggregate has to reach `GROUP BY`
//! and `ORDER BY`, a registration has to override a built-in of the same name,
//! and a collation has to change what an `ORDER BY` returns rather than only
//! what a comparison answers.

use std::sync::Arc;

use inillucent_compat::facade::Database;
use inillucent_ext::registry::FunctionFlags;
use inillucent_value::Value;

/// Returns the first column of the first row, as an integer.
fn single(connection: &inillucent_compat::facade::Connection, sql: &str) -> Option<i64> {
    let rows = connection.query(sql).expect("runs");
    rows.first()
        .and_then(|row| row.first())
        .and_then(Value::as_integer)
}

/// Returns a database file of this test's own, under the gitignored root.
///
/// **A file rather than `:memory:`, which the old facade accepted.** The new
/// engine opens a path and has no in-memory VFS behind `open` yet; nothing in
/// this file asserts anything about *where* the database lives, so the fixture
/// moves and every assertion stays exactly as it was. A serial keeps two tests
/// running in parallel from colliding on one file.
fn scratch() -> std::path::PathBuf {
    let root = inillucent_compat::workspace_root().join("_agent_output/task-1838/functions");
    let _ = std::fs::create_dir_all(&root);
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = root.join(format!("{}-{serial}.rdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// Opens a database of this test's own, with one connection.
fn connect() -> inillucent_compat::facade::Connection {
    let database = Database::open(scratch()).expect("opens");
    Box::leak(Box::new(database)).connect().expect("connects")
}

/// A scalar an application registered is callable from SQL.
#[test]
fn a_registered_scalar_is_callable() {
    let connection = connect();
    connection
        .create_scalar_function(
            "twice",
            1,
            FunctionFlags::external(),
            Arc::new(|arguments: &[Value<'static>]| {
                let value = arguments.first().and_then(Value::as_integer).unwrap_or(0);
                Ok(Value::Integer(value * 2))
            }),
        )
        .expect("registers");
    assert_eq!(single(&connection, "SELECT twice(21)"), Some(42));
}

/// A registration replaces a built-in of the same name and arity.
#[test]
fn a_registration_overrides_a_builtin() {
    let connection = connect();
    connection
        .create_scalar_function(
            "abs",
            1,
            FunctionFlags::external(),
            Arc::new(|_| Ok(Value::Integer(-1))),
        )
        .expect("registers");
    assert_eq!(single(&connection, "SELECT abs(-5)"), Some(-1));
}

/// An aggregate reduces a group, and `GROUP BY` splits the groups.
#[test]
fn a_registered_aggregate_reduces_a_group() {
    let connection = connect();
    connection
        .create_aggregate_function(
            "total2",
            1,
            FunctionFlags::external(),
            Arc::new(|rows: &[Vec<Value<'static>>]| {
                let sum: i64 = rows
                    .iter()
                    .filter_map(|row| row.first())
                    .filter_map(Value::as_integer)
                    .sum();
                Ok(Value::Integer(sum))
            }),
        )
        .expect("registers");
    connection
        .execute_batch("CREATE TABLE t(g, n); INSERT INTO t VALUES (1,10),(1,5),(2,7)")
        .expect("fills");
    let rows = connection
        .query("SELECT g, total2(n) FROM t GROUP BY g ORDER BY g")
        .expect("runs");
    let totals: Vec<i64> = rows
        .iter()
        .filter_map(|row| row.get(1))
        .filter_map(Value::as_integer)
        .collect();
    assert_eq!(totals, vec![15, 7]);
}

/// A collation an application defines changes what `ORDER BY` returns.
#[test]
fn a_registered_collation_orders_rows() {
    let connection = connect();
    connection
        .create_collation(
            "BYLEN",
            Arc::new(|left: &[u8], right: &[u8]| {
                left.len().cmp(&right.len()).then_with(|| left.cmp(right))
            }),
        )
        .expect("registers");
    connection
        .execute_batch("CREATE TABLE w(x); INSERT INTO w VALUES ('bbb'),('a'),('cc')")
        .expect("fills");
    let rows = connection
        .query("SELECT x FROM w ORDER BY x COLLATE BYLEN")
        .expect("runs");
    let order: Vec<String> = rows
        .iter()
        .filter_map(|row| row.first())
        .filter_map(Value::as_text)
        .map(|text| String::from_utf8_lossy(text.raw()).into_owned())
        .collect();
    assert_eq!(order, vec!["a", "cc", "bbb"]);
}

/// A collation a column declares is used by the index built over it.
#[test]
fn a_declared_collation_reaches_the_column() {
    let connection = connect();
    connection
        .create_collation(
            "REVERSED",
            Arc::new(|left: &[u8], right: &[u8]| right.cmp(left)),
        )
        .expect("registers");
    connection
        .execute_batch(
            "CREATE TABLE w(x TEXT COLLATE REVERSED); INSERT INTO w VALUES ('a'),('b'),('c')",
        )
        .expect("fills");
    let rows = connection
        .query("SELECT x FROM w ORDER BY x")
        .expect("runs");
    let order: Vec<String> = rows
        .iter()
        .filter_map(|row| row.first())
        .filter_map(Value::as_text)
        .map(|text| String::from_utf8_lossy(text.raw()).into_owned())
        .collect();
    assert_eq!(order, vec!["c", "b", "a"]);
}

/// Removing a function makes its name unknown again.
#[test]
fn a_removed_function_is_unknown_again() {
    let connection = connect();
    connection
        .create_scalar_function(
            "gone",
            0,
            FunctionFlags::external(),
            Arc::new(|_| Ok(Value::Integer(1))),
        )
        .expect("registers");
    assert!(connection.query("SELECT gone()").is_ok());
    assert!(
        connection.remove_function("gone", 0),
        "the function was removed"
    );
    let refused = connection.query("SELECT gone()");
    assert!(refused.is_err(), "the name should be unknown again");
}
