//! Read-only SQL, end to end, against databases SQLite wrote.
//!
//! Invariant: every database these tests query was created by the pinned
//! SQLite 3.53.4 binary, and none of them is modified by being read. A test
//! that passed against a file inillucent had also written would be testing inillucent
//! against itself.

use std::path::PathBuf;

use inillucent_compat::fixtures::valid_fixtures;
use inillucent_compat::workspace_root;
use inillucent_legacy::{Database, Value};

/// Returns the path of a shipped fixture.
fn fixture(name: &str) -> PathBuf {
    workspace_root().join("compat/fixtures").join(name)
}

/// Opens a fixture and returns a connection.
///
/// The timeout is not decoration. These tests run in parallel threads against
/// the same files, and Windows takes the read lock in two steps serialised by
/// the PENDING byte, so two readers starting together collide on it even though
/// neither is a writer. A real application sets a busy timeout for the same
/// reason.
fn open(name: &str) -> Database {
    Database::open_with_busy_timeout(fixture(name), std::time::Duration::from_secs(5))
        .expect("the fixture opens")
}

/// Opens a fixture and returns a connection.
fn connect(name: &str) -> inillucent_legacy::Connection {
    let database = open(name);
    database.connect().expect("the connection opens")
}

/// Renders a row the way the tests compare it: a tagged string per value, so a
/// difference in storage class is a difference in the text.
fn render(values: &[Value<'static>]) -> String {
    values
        .iter()
        .map(|value| match value {
            Value::Null => "null".to_string(),
            Value::Integer(integer) => format!("int:{integer}"),
            Value::Real(real) => format!("real:{real:?}"),
            Value::Text(text) => {
                format!("text:{}", String::from_utf8_lossy(&text.utf8_bytes()))
            }
            Value::Blob(blob) => format!("blob:{}", hex(blob.raw())),
        })
        .collect::<Vec<String>>()
        .join("|")
}

/// Renders bytes as hexadecimal.
fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<String>>()
        .join("")
}

/// Runs a query and renders every row.
fn rows(connection: &inillucent_legacy::Connection, sql: &str) -> Vec<String> {
    let mut statement = connection.prepare(sql).expect(sql);
    let mut out = Vec::new();
    while statement.step().expect(sql) {
        out.push(render(statement.row()));
    }
    out
}

/// A scan returns every row with the storage class each value was written with.
///
/// The order is the one the *plan* produces, not rowid order, and the two are
/// not the same once a covering index can answer the query: `people` has three
/// indexes and `id, name` is carried by two of them, so both engines read the
/// narrower structure instead of the table. The order below is `people_nocase`
/// order, and it is what the pinned SQLite 3.53.4 returns for this query on
/// this fixture - checked, not assumed. SQL promises no order without an
/// `ORDER BY`, so what this asserts is the storage classes and the fact that
/// the two engines agree about which path to take.
#[test]
fn a_full_scan_returns_every_row_the_plan_produces() {
    let connection = connect("basic-p4096-utf8.db");
    let found = rows(&connection, "SELECT id, name FROM people");
    assert_eq!(
        found,
        vec![
            "int:3|text:",
            "int:1|text:alpha",
            "int:9007199254740993|text:big rowid",
            "int:2|text:Bravo",
            "int:7|text:delta echo",
            "int:100|text:héllo ☃ 😀",
            "int:-5|text:negative rowid",
        ]
    );
}

/// The five storage classes survive the round trip out of a real file.
#[test]
fn every_storage_class_reads_back() {
    let connection = connect("basic-p4096-utf8.db");
    let found = rows(
        &connection,
        "SELECT id, name, score, tag, note FROM people WHERE id = 1",
    );
    assert_eq!(
        found,
        vec!["int:1|text:alpha|real:1.5|blob:00ff|text:first"]
    );
    let nulls = rows(&connection, "SELECT note FROM people WHERE id = 2");
    assert_eq!(nulls, vec!["null"]);
}

/// A `WHERE` clause on the rowid becomes a seek, and finds the one row.
#[test]
fn a_rowid_equality_becomes_a_seek() {
    let connection = connect("basic-p4096-utf8.db");
    let mut statement = connection
        .prepare("SELECT name FROM people WHERE id = 7")
        .expect("it prepares");
    let explained = statement.explain().join("\n");
    assert!(explained.contains("SeekRowid"), "{explained}");
    let mut found = Vec::new();
    while statement.step().expect("it steps") {
        found.push(render(statement.row()));
    }
    assert_eq!(found, vec!["text:delta echo"]);
}

/// A rowid range becomes a positioned scan that stops at the upper bound.
#[test]
fn a_rowid_range_stops_at_its_bound() {
    let connection = connect("basic-p4096-utf8.db");
    let found = rows(
        &connection,
        "SELECT id FROM people WHERE id > 1 AND id <= 7",
    );
    assert_eq!(found, vec!["int:2", "int:3", "int:7"]);
}

/// An indexed column becomes an index seek, and returns the same rows a scan
/// would.
#[test]
fn an_index_seek_returns_what_a_scan_returns() {
    let connection = connect("basic-p4096-utf8.db");
    let mut statement = connection
        .prepare("SELECT id FROM people WHERE name = 'alpha'")
        .expect("it prepares");
    let explained = statement.explain().join("\n");
    assert!(explained.contains("OpenIndex"), "{explained}");
    let mut found = Vec::new();
    while statement.step().expect("it steps") {
        found.push(render(statement.row()));
    }
    assert_eq!(found, vec!["int:1"]);

    // The same question, forced onto a scan by wrapping the column so the
    // planner cannot use the index, must give the same answer.
    let scanned = rows(
        &connection,
        "SELECT id FROM people WHERE name || '' = 'alpha'",
    );
    assert_eq!(found, scanned);
}

/// `ORDER BY` sorts, `LIMIT` truncates, and `OFFSET` skips.
#[test]
fn order_limit_and_offset_work_together() {
    let connection = connect("basic-p4096-utf8.db");
    assert_eq!(
        rows(
            &connection,
            "SELECT id FROM people ORDER BY id DESC LIMIT 3"
        ),
        vec!["int:9007199254740993", "int:100", "int:7"]
    );
    assert_eq!(
        rows(
            &connection,
            "SELECT id FROM people ORDER BY id LIMIT 2 OFFSET 2"
        ),
        vec!["int:2", "int:3"]
    );
    // The comma form reverses its operands, which is the trap in this syntax.
    assert_eq!(
        rows(&connection, "SELECT id FROM people ORDER BY id LIMIT 2, 2"),
        vec!["int:2", "int:3"]
    );
    assert!(rows(&connection, "SELECT id FROM people LIMIT 0").is_empty());
}

/// NULLs sort first ascending and last descending, unless told otherwise.
#[test]
fn nulls_sort_where_sqlite_sorts_them() {
    let connection = connect("basic-p4096-utf8.db");
    assert_eq!(
        rows(&connection, "SELECT note FROM people ORDER BY note LIMIT 1"),
        vec!["null"]
    );
    assert_eq!(
        rows(
            &connection,
            "SELECT note FROM people ORDER BY note DESC LIMIT 1"
        ),
        vec!["text:third"]
    );
    assert_eq!(
        rows(
            &connection,
            "SELECT note FROM people ORDER BY note NULLS LAST LIMIT 1"
        ),
        vec!["int:-9223372036854775808"]
    );
}

/// Aggregates over the whole table, including the empty-input answers.
#[test]
fn aggregates_answer_over_the_whole_table() {
    let connection = connect("basic-p4096-utf8.db");
    assert_eq!(
        rows(&connection, "SELECT count(*) FROM people"),
        vec!["int:7"]
    );
    assert_eq!(
        rows(&connection, "SELECT count(note) FROM people"),
        vec!["int:6"]
    );
    assert_eq!(
        rows(
            &connection,
            "SELECT count(*) FROM people WHERE id > 1000000"
        ),
        vec!["int:0"]
    );
    assert_eq!(
        rows(&connection, "SELECT sum(id) FROM people WHERE id > 1000000"),
        vec!["null"]
    );
    assert_eq!(
        rows(&connection, "SELECT min(id), max(id) FROM people"),
        vec!["int:-5|int:9007199254740993"]
    );
}

/// `GROUP BY` groups, and `HAVING` filters the groups.
#[test]
fn group_by_groups_and_having_filters() {
    let connection = connect("basic-p4096-utf8.db");
    let found = rows(
        &connection,
        "SELECT note IS NULL, count(*) FROM people GROUP BY note IS NULL",
    );
    assert_eq!(found, vec!["int:0|int:6", "int:1|int:1"]);
    let filtered = rows(
        &connection,
        "SELECT note IS NULL, count(*) FROM people GROUP BY note IS NULL HAVING count(*) > 3",
    );
    assert_eq!(filtered, vec!["int:0|int:6"]);
}

/// `DISTINCT` emits each row once, in first-seen order.
#[test]
fn distinct_emits_each_row_once() {
    let connection = connect("basic-p4096-utf8.db");
    let found = rows(&connection, "SELECT DISTINCT note IS NULL FROM people");
    assert_eq!(found, vec!["int:0", "int:1"]);
}

/// `VALUES` runs with no table at all.
#[test]
fn values_runs_without_a_table() {
    let connection = connect("basic-p4096-utf8.db");
    assert_eq!(
        rows(&connection, "VALUES (1, 'a'), (2, 'b')"),
        vec!["int:1|text:a", "int:2|text:b"]
    );
    assert_eq!(
        rows(&connection, "SELECT 1 + 1, 'x' || 'y', NULL"),
        vec!["int:2|text:xy|null"]
    );
}

/// Parameters bind by index and by name, and rebinding after a reset works.
#[test]
fn parameters_bind_and_rebind() {
    let connection = connect("basic-p4096-utf8.db");
    let mut statement = connection
        .prepare("SELECT name FROM people WHERE id = ?1")
        .expect("it prepares");
    statement.bind_integer(1, 7).expect("it binds");
    assert!(statement.step().expect("it steps"));
    assert_eq!(render(statement.row()), "text:delta echo");
    statement.reset().expect("it resets");
    statement.bind_integer(1, 1).expect("it rebinds");
    assert!(statement.step().expect("it steps"));
    assert_eq!(render(statement.row()), "text:alpha");
}

/// Result metadata names the column and its origin.
#[test]
fn result_metadata_names_the_column_and_its_origin() {
    let connection = connect("basic-p4096-utf8.db");
    let statement = connection
        .prepare("SELECT name, score AS s, id + 1 FROM people")
        .expect("it prepares");
    let names: Vec<String> = statement
        .columns()
        .iter()
        .map(|column| String::from_utf8_lossy(&column.name).into_owned())
        .collect();
    // An unaliased expression is named after the text it was written as,
    // which is SQLite's default and what `sqlite3_column_name` reports.
    assert_eq!(names, vec!["name", "s", "id + 1"]);
    let origin = statement
        .columns()
        .first()
        .and_then(|column| column.origin.clone())
        .map(|(database, table, column)| {
            format!(
                "{}.{}.{}",
                String::from_utf8_lossy(&database),
                String::from_utf8_lossy(&table),
                String::from_utf8_lossy(&column)
            )
        });
    assert_eq!(origin, Some("main.people.name".to_string()));
    assert_eq!(
        statement
            .columns()
            .first()
            .map(|column| String::from_utf8_lossy(&column.declared_type).into_owned()),
        Some("TEXT".to_string())
    );
}

/// A name nobody declared is an error at prepare time, with an offset.
#[test]
fn an_unknown_name_fails_at_prepare_with_an_offset() {
    let connection = connect("basic-p4096-utf8.db");
    let missing_table = match connection.prepare("SELECT * FROM nope") {
        Err(failure) => failure,
        Ok(_) => panic!("a missing table must not prepare"),
    };
    assert!(
        missing_table.message().contains("no such table"),
        "{missing_table}"
    );
    let missing_column = match connection.prepare("SELECT nope FROM people") {
        Err(failure) => failure,
        Ok(_) => panic!("a missing column must not prepare"),
    };
    assert!(
        missing_column.message().contains("no such column"),
        "{missing_column}"
    );
    let syntax = match connection.prepare("SELECT FROM") {
        Err(failure) => failure,
        Ok(_) => panic!("a syntax error must not prepare"),
    };
    assert_eq!(syntax.sql_offset(), Some(7));
}

/// Reading a database changes no byte of it, which is the promise the whole
/// read-only engine rests on.
#[test]
fn reading_changes_no_byte_of_the_file() {
    for fixture_row in valid_fixtures() {
        let path = fixture(fixture_row.name);
        if !path.is_file() {
            continue;
        }
        let before = std::fs::read(&path).expect("the fixture reads");
        {
            let database =
                Database::open_with_busy_timeout(&path, std::time::Duration::from_secs(5))
                    .expect("it opens");
            let connection = database.connect().expect("it connects");
            // Whatever the fixture holds, scanning `sqlite_schema` through the
            // catalog and running one query over it is enough to touch the
            // pager, the cache and a cursor.
            let _ = connection.query("SELECT count(*) FROM sqlite_schema");
        }
        let after = std::fs::read(&path).expect("the fixture reads");
        assert_eq!(before, after, "{} changed", fixture_row.name);
        let journal = path.with_extension("db-journal");
        assert!(!journal.exists(), "{} left a journal", fixture_row.name);
    }
}

/// Every page size and text encoding the fixtures cover reads the same rows.
#[test]
fn every_page_size_and_encoding_reads_the_same_rows() {
    let mut baseline: Option<Vec<String>> = None;
    for name in [
        "basic-p512-utf8.db",
        "basic-p1024-utf8.db",
        "basic-p4096-utf8.db",
        "basic-p65536-utf8.db",
        "basic-p1024-utf16le.db",
        "basic-p1024-utf16be.db",
    ] {
        let path = fixture(name);
        if !path.is_file() {
            continue;
        }
        let database = Database::open_with_busy_timeout(&path, std::time::Duration::from_secs(5))
            .expect("it opens");
        let connection = database.connect().expect("it connects");
        let found = rows(
            &connection,
            "SELECT id, name, score FROM people ORDER BY id",
        );
        match &baseline {
            None => baseline = Some(found),
            Some(expected) => assert_eq!(&found, expected, "{name}"),
        }
    }
    assert!(baseline.is_some(), "no fixture was readable");
}
