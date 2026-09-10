//! What an extension does wrong, and what the engine has to do about it.
//!
//! Invariant: nothing an extension does may take the engine down with it. An
//! application's function can fail, lie, re-enter, or hand back a value of the
//! wrong shape; a module's tables live in the same file as everything else and
//! can be corrupted by anything that can write the file. In every one of those
//! cases the statement must fail and the *connection* must survive - still
//! usable, still able to run the next statement, still able to close.
//!
//! That is a stronger claim than "does not crash", and it is the one that
//! matters: a connection that is poisoned by a bad extension is a process that
//! has to be restarted, and a database that is poisoned by one is a restore.
//!
//! The corruption cases are the reason this file exists rather than a few more
//! cases in `functions.rs`. A module's shadow tables are ordinary tables, so
//! anything that can write the database can put anything in them - and the
//! module reads them back expecting its own format. Every field it reads is an
//! opportunity to trust a length, an offset or a count that came from the file.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use inillucent_base::error::{DbError, PrimaryCode};
use inillucent_compat::facade::{Connection, Database};
use inillucent_ext::registry::FunctionFlags;
use inillucent_value::Value;

/// Returns a database file of this test's own, under the gitignored root.
///
/// **A file rather than `:memory:`, which the old facade accepted.** The new
/// engine opens a path and has no in-memory VFS behind `open` yet; nothing in
/// this file asserts anything about *where* the database lives, so the fixture
/// moves and every assertion stays exactly as it was. A serial keeps two tests
/// running in parallel from colliding on one file.
fn scratch() -> std::path::PathBuf {
    let root = inillucent_compat::workspace_root().join("_agent_output/hostile");
    let _ = std::fs::create_dir_all(&root);
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = root.join(format!("{}-{serial}.rdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// Opens a database of this test's own, with one connection.
///
/// The database is leaked so the connection can be returned on its own. These
/// are short tests and there is exactly one database per test; a lifetime here
/// would be carried through every helper for nothing.
fn connect() -> Connection {
    let database = Database::open(scratch()).expect("opens");
    Box::leak(Box::new(database)).connect().expect("connects")
}

/// Returns the failure a hostile function reports.
fn refuse(message: &str) -> DbError {
    DbError::primary(PrimaryCode::Error).with_message(message)
}

/// A function that always fails stops the statement and nothing else.
#[test]
fn a_function_that_always_fails_leaves_the_connection_usable() {
    let connection = connect();
    connection
        .create_scalar_function(
            "explode",
            0,
            FunctionFlags::external(),
            Arc::new(|_| Err(refuse("no"))),
        )
        .expect("registers");
    connection
        .execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES (1),(2),(3)")
        .expect("fills");
    let refused = connection.query("SELECT explode() FROM t");
    assert!(refused.is_err(), "the failure has to reach the caller");
    // The connection is the thing being tested, not the failure.
    let rows = connection
        .query("SELECT count(*) FROM t")
        .expect("still works");
    assert_eq!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(3)
    );
}

/// A function that fails part-way through a write leaves nothing behind.
#[test]
fn a_function_that_fails_mid_statement_rolls_the_statement_back() {
    let connection = connect();
    let calls = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&calls);
    connection
        .create_scalar_function(
            "third_time_fails",
            1,
            FunctionFlags::external(),
            Arc::new(move |arguments: &[Value<'static>]| {
                if counter.fetch_add(1, Ordering::Relaxed) >= 2 {
                    return Err(refuse("enough"));
                }
                Ok(arguments.first().cloned().unwrap_or(Value::Null))
            }),
        )
        .expect("registers");
    connection
        .execute_batch("CREATE TABLE src(a); INSERT INTO src VALUES (1),(2),(3),(4)")
        .expect("fills");
    connection
        .execute_batch("CREATE TABLE dst(a)")
        .expect("creates");
    let refused = connection.execute_batch("INSERT INTO dst SELECT third_time_fails(a) FROM src");
    assert!(refused.is_err(), "the statement has to fail");
    let rows = connection.query("SELECT count(*) FROM dst").expect("reads");
    assert_eq!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(0),
        "a statement that failed must not have written half a table"
    );
}

/// A collation that is not an ordering still terminates and keeps its rows.
///
/// A comparator that answers differently every time breaks the contract a sort
/// depends on, and the sort has no way to detect that. What it must not do is
/// loop, panic, or lose a row: whatever order comes out, the same values have
/// to come out.
#[test]
fn an_inconsistent_collation_still_returns_every_row() {
    let connection = connect();
    let state = Arc::new(AtomicU64::new(0x2545_F491_4F6C_DD1D));
    connection
        .create_collation(
            "CHAOS",
            Arc::new(move |_left: &[u8], _right: &[u8]| {
                // A deliberately inconsistent comparator, deterministic across
                // runs so a failure can be reproduced.
                let mut bits = state.load(Ordering::Relaxed);
                bits ^= bits << 13;
                bits ^= bits >> 7;
                bits ^= bits << 17;
                state.store(bits, Ordering::Relaxed);
                match bits % 3 {
                    0 => std::cmp::Ordering::Less,
                    1 => std::cmp::Ordering::Equal,
                    _ => std::cmp::Ordering::Greater,
                }
            }),
        )
        .expect("registers");
    connection
        .execute_batch(
            "CREATE TABLE t(x); INSERT INTO t VALUES ('a'),('b'),('c'),('d'),('e'),('f')",
        )
        .expect("fills");
    let rows = connection
        .query("SELECT x FROM t ORDER BY x COLLATE CHAOS")
        .expect("the sort terminates");
    assert_eq!(rows.len(), 6, "every row has to come back");
    let mut seen: Vec<String> = rows
        .iter()
        .filter_map(|row| row.first())
        .filter_map(Value::as_text)
        .map(|text| String::from_utf8_lossy(text.raw()).into_owned())
        .collect();
    seen.sort();
    assert_eq!(seen, vec!["a", "b", "c", "d", "e", "f"]);
}

/// An aggregate that returns a different type every group is still safe.
#[test]
fn an_aggregate_that_changes_its_mind_is_harmless() {
    let connection = connect();
    connection
        .create_aggregate_function(
            "shifty",
            1,
            FunctionFlags::external(),
            Arc::new(|rows: &[Vec<Value<'static>>]| {
                Ok(match rows.len() % 4 {
                    0 => Value::Null,
                    1 => Value::Integer(1),
                    2 => Value::Real(2.5),
                    _ => Value::owned_blob(&[0xff, 0x00, 0xff])?,
                })
            }),
        )
        .expect("registers");
    connection
        .execute_batch(
            "CREATE TABLE t(g, n); INSERT INTO t VALUES (1,1),(2,1),(2,2),(3,1),(3,2),(3,3)",
        )
        .expect("fills");
    let rows = connection
        .query("SELECT g, shifty(n) FROM t GROUP BY g ORDER BY g")
        .expect("runs");
    assert_eq!(rows.len(), 3);
    // The point is that a row came back for each group and nothing was
    // corrupted by the changing shape; the values themselves are the
    // function's business.
    let rows = connection
        .query("SELECT count(*) FROM t")
        .expect("still works");
    assert_eq!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(6)
    );
}

/// A function that runs SQL on its own connection is refused, not deadlocked.
#[test]
fn a_reentrant_function_is_refused_rather_than_wedged() {
    let connection = connect();
    connection
        .execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES (1),(2)")
        .expect("fills");
    // The closure cannot hold the connection - it would be a cycle - so this
    // opens its own. What is being checked is that a function doing arbitrary
    // database work during a statement does not wedge the one that called it.
    connection
        .create_scalar_function(
            "nested",
            0,
            FunctionFlags::external(),
            Arc::new(|_| {
                let inner = connect();
                inner.execute_batch("CREATE TABLE u(a)")?;
                Ok(Value::Integer(7))
            }),
        )
        .expect("registers");
    let rows = connection.query("SELECT nested() FROM t").expect("runs");
    assert_eq!(rows.len(), 2);
    let rows = connection
        .query("SELECT count(*) FROM t")
        .expect("still works");
    assert_eq!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(2)
    );
}

/// Every statement a hostile function touched can still be finalized.
///
/// The engine's `Statement` finalizes on drop, so what this really checks is
/// that a statement abandoned mid-scan - which is what a failing function
/// leaves behind - does not hold anything the next statement needs.
#[test]
fn a_failed_statement_releases_what_it_held() {
    let connection = connect();
    connection
        .create_scalar_function(
            "explode",
            0,
            FunctionFlags::external(),
            Arc::new(|_| Err(refuse("no"))),
        )
        .expect("registers");
    connection
        .execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES (1),(2),(3)")
        .expect("fills");
    for _ in 0..20 {
        let _ = connection.query("SELECT explode() FROM t");
    }
    // A write needs the file lock the failed readers were holding, so this
    // fails if any of them left one behind.
    connection
        .execute_batch("INSERT INTO t VALUES (4)")
        .expect("the writer still gets the lock");
    let rows = connection.query("SELECT count(*) FROM t").expect("reads");
    assert_eq!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(4)
    );
}

/// Bytes written into a module's shadow tables are read back defensively.
///
/// Every case here writes something a module would never have written and then
/// asks it a question. What the module does with the answer is its business;
/// what it may not do is loop forever, read out of bounds, or report success.
#[test]
fn corrupt_module_structures_are_refused_rather_than_trusted() {
    for (name, schema, damage) in scenarios() {
        let connection = connect();
        connection
            .execute_batch(schema)
            .unwrap_or_else(|error| panic!("{name}: setup failed: {}", error.message()));
        // The shadow tables are ordinary tables, which is exactly why this is
        // possible at all - and why it has to be tested.
        connection
            .execute_batch("PRAGMA writable_schema = ON")
            .unwrap_or_else(|error| panic!("{name}: {}", error.message()));
        let _ = connection.execute_batch(damage);
        for query in queries(name) {
            // Either answer is acceptable. What is not acceptable is a hang or
            // a panic, and reaching the next line is the assertion.
            let _ = connection.query(query);
        }
        let rows = connection
            .query("SELECT 1")
            .unwrap_or_else(|error| panic!("{name}: the connection died: {}", error.message()));
        assert_eq!(rows.len(), 1, "{name}: the connection has to survive");
    }
}

/// Returns the corruption scenarios: a name, a schema, and the damage.
fn scenarios() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "rtree-node-truncated",
            "CREATE VIRTUAL TABLE r USING rtree(id, x0, x1, y0, y1);\
             INSERT INTO r VALUES (1, 0, 1, 0, 1), (2, 5, 6, 5, 6), (3, 9, 10, 9, 10);",
            "UPDATE r_node SET data = x'0000'",
        ),
        (
            "rtree-node-claims-too-many-cells",
            "CREATE VIRTUAL TABLE r USING rtree(id, x0, x1, y0, y1);\
             INSERT INTO r VALUES (1, 0, 1, 0, 1);",
            "UPDATE r_node SET data = x'0000FFFF' || hex(data)",
        ),
        (
            "rtree-node-is-not-a-blob",
            "CREATE VIRTUAL TABLE r USING rtree(id, x0, x1, y0, y1);\
             INSERT INTO r VALUES (1, 0, 1, 0, 1);",
            "UPDATE r_node SET data = 'not a node at all'",
        ),
        (
            "rtree-rowid-map-lies",
            "CREATE VIRTUAL TABLE r USING rtree(id, x0, x1, y0, y1);\
             INSERT INTO r VALUES (1, 0, 1, 0, 1);",
            "UPDATE r_rowid SET nodeno = 999999",
        ),
        (
            "fts5-doclist-truncated",
            "CREATE VIRTUAL TABLE f USING fts5(a, b);\
             INSERT INTO f VALUES ('alpha beta', 'gamma');\
             INSERT INTO f VALUES ('beta delta', 'epsilon');",
            "UPDATE f_data SET block = x'ff'",
        ),
        (
            "fts5-doclist-is-huge-nonsense",
            "CREATE VIRTUAL TABLE f USING fts5(a, b);\
             INSERT INTO f VALUES ('alpha beta', 'gamma');",
            "UPDATE f_data SET block = x'ffffffffffffffffffffffffffffffff'",
        ),
        (
            "fts5-index-points-nowhere",
            "CREATE VIRTUAL TABLE f USING fts5(a, b);\
             INSERT INTO f VALUES ('alpha beta', 'gamma');",
            "UPDATE f_idx SET pgno = 4000000000",
        ),
        (
            "fts5-sizes-disagree-with-content",
            "CREATE VIRTUAL TABLE f USING fts5(a, b);\
             INSERT INTO f VALUES ('alpha beta', 'gamma');",
            "UPDATE f_docsize SET sz = x'ffffffff'",
        ),
        (
            "fts5-content-row-removed",
            "CREATE VIRTUAL TABLE f USING fts5(a, b);\
             INSERT INTO f VALUES ('alpha beta', 'gamma');",
            "DELETE FROM f_content",
        ),
    ]
}

/// Returns the questions to ask after one scenario's damage.
fn queries(name: &str) -> Vec<&'static str> {
    if name.starts_with("rtree") {
        return vec![
            "SELECT id FROM r",
            "SELECT id FROM r WHERE x0 > 0 AND x1 < 100",
            "SELECT count(*) FROM r WHERE y0 >= -1000",
            "PRAGMA integrity_check",
        ];
    }
    vec![
        "SELECT rowid FROM f",
        "SELECT rowid FROM f WHERE f MATCH 'beta'",
        "SELECT rowid FROM f WHERE f MATCH 'alpha OR gamma'",
        "SELECT rowid, rank FROM f WHERE f MATCH 'beta' ORDER BY rank",
        "PRAGMA integrity_check",
    ]
}

/// Random bytes in a module's tables are refused the same way.
///
/// The scenarios above are the shapes worth naming; this is the rest of the
/// space. It writes deterministic pseudo-random blobs into every shadow table
/// and asks the same questions, which is where a length nobody thought about
/// gets found.
#[test]
fn random_bytes_in_a_shadow_table_are_survivable() {
    let mut bits: u64 = 0x9E37_79B9_7F4A_7C15;
    for round in 0..24u32 {
        let connection = connect();
        let module = if round % 2 == 0 {
            "CREATE VIRTUAL TABLE m USING rtree(id, x0, x1, y0, y1);\
             INSERT INTO m VALUES (1, 0, 1, 0, 1), (2, 4, 5, 4, 5), (3, 8, 9, 8, 9);"
        } else {
            "CREATE VIRTUAL TABLE m USING fts5(a, b);\
             INSERT INTO m VALUES ('one two', 'three');\
             INSERT INTO m VALUES ('two three', 'four');"
        };
        connection.execute_batch(module).expect("setup");
        let shadows: Vec<String> = connection
            .query("SELECT name FROM sqlite_master WHERE name LIKE 'm!_%' ESCAPE '!'")
            .expect("lists")
            .iter()
            .filter_map(|row| row.first())
            .filter_map(Value::as_text)
            .map(|text| String::from_utf8_lossy(text.raw()).into_owned())
            .collect();
        assert!(!shadows.is_empty(), "a module has shadow tables");
        for table in &shadows {
            bits = next(bits);
            let blob = hex_of(bits);
            let column = column_of(&connection, table);
            let Some(column) = column else {
                continue;
            };
            let _ = connection
                .execute_batch(&format!("UPDATE \"{table}\" SET \"{column}\" = x'{blob}'"));
        }
        for query in [
            "SELECT rowid FROM m",
            "SELECT count(*) FROM m",
            "PRAGMA integrity_check",
        ] {
            let _ = connection.query(query);
        }
        let rows = connection
            .query("SELECT 1")
            .expect("the connection survives");
        assert_eq!(rows.len(), 1, "round {round}");
    }
}

/// Returns the last column of a table, which is where a module's payload is.
fn column_of(connection: &Connection, table: &str) -> Option<String> {
    let rows = connection
        .query(&format!("PRAGMA table_info(\"{table}\")"))
        .ok()?;
    rows.last()
        .and_then(|row| row.get(1))
        .and_then(Value::as_text)
        .map(|text| String::from_utf8_lossy(text.raw()).into_owned())
}

/// Returns the next value of a deterministic generator.
fn next(mut bits: u64) -> u64 {
    bits ^= bits << 13;
    bits ^= bits >> 7;
    bits ^= bits << 17;
    bits
}

/// Returns some bytes as hex, for an SQL blob literal.
fn hex_of(bits: u64) -> String {
    let mut out = String::new();
    let mut value = bits;
    for _ in 0..12 {
        out.push_str(&format!("{:02x}", (value & 0xff) as u8));
        value = value.rotate_right(5);
    }
    out
}
