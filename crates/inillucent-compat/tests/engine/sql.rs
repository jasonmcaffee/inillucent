//! Read-only SQL, end to end, against databases SQLite wrote.
//!
//! Invariant: every database these tests query was created by the pinned
//! SQLite 3.53.4 binary, and none of them is modified by being read. A test
//! that passed against a file inillucent had also written would be testing inillucent
//! against itself.
//!
//! **Opened through `Database::import`, not `Database::open`.** File-format
//! compatibility was never kept when the new engine was built: `open`ing a
//! file SQLite wrote reports that neither meta page is readable, which is
//! correct - it is not this engine's format. `import` is the one route a
//! SQLite file reaches this engine by, reading it through
//! `inillucent-sqlite-reader` and rebuilding it as PAX trees beside the source,
//! `<source>.rdb`. The source is a copy in this process's scratch folder (see
//! `stage`), so neither the fixture nor its folder is written to. Every plan
//! string these tests used to check for a bytecode opcode name -
//! `SeekRowid`, `OpenIndex` - now checks for the operator chain's own prose,
//! `SEARCH t USING INTEGER PRIMARY KEY (rowid=?)` and `USING ... INDEX`,
//! which `tests/new_engine_explain.rs` establishes against the same pinned
//! reference.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use inillucent_compat::fixtures::valid_fixtures;
use inillucent_compat::workspace_root;
use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

/// Returns the path of a shipped fixture.
fn fixture(name: &str) -> PathBuf {
    workspace_root().join("compat/fixtures").join(name)
}

/// Copies a shipped fixture into this process's scratch folder and returns the
/// copy.
///
/// **The import runs on the copy, never in `compat/fixtures`.** An import writes
/// `<source>.rdb` beside its source. Done in place, that put a build product in
/// a tracked folder, and `engine::storage::reading_changes_nothing_on_disk`
/// hashes that folder from another process: the nightly of 2026-09-25 failed
/// when this suite rebuilt `basic-p1024-utf8.db.rdb` between that test's two
/// hashes. The folder is named for the process, so two runs of this binary do
/// not share it either.
///
/// @param name - the fixture's file name
fn stage(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("inillucent-engine-sql-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("the scratch folder is made");
    let staged = directory.join(name);
    std::fs::copy(fixture(name), &staged).expect("the fixture is staged");
    staged
}

/// Imports a fixture exactly once for the whole test binary and returns the
/// file it was imported to.
///
/// **Every test in this file that needs `name` goes through this rather than
/// calling `Database::import` itself.** `Database::import`'s target is fixed
/// at `<source>.rdb` - one path per fixture name - and libtest runs every
/// `#[test]` on its own thread, so the thirteen tests that all read
/// `basic-p4096-utf8.db` used to race rewriting that single file: whichever
/// ran second either removed the file out from under the first's open
/// connection or tried to open it while the first still held a lock, and
/// that surfaced as a SQLite-style lock refusal (`another connection holds
/// RESERVED`, `readers are still present`) rather than as anything about the
/// engine's SQL. `tests/lifecycle.rs` hit the identical shape against
/// `select-corpus.db` and fixed it by importing once behind a `OnceLock`;
/// this generalises that to every fixture name this file uses, keyed by name
/// in one `Mutex`-guarded map so two tests never import the same name twice.
/// A first import of one name still blocks a first import of another behind
/// the same lock, which only costs an import's own time and never a wrong
/// answer, so it is not worth a lock per name.
fn imported_path(name: &str) -> PathBuf {
    static IMPORTED: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    let cache = IMPORTED.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().expect("the fixture cache is not poisoned");
    if let Some(path) = cache.get(name) {
        return path.clone();
    }
    let source = stage(name);
    let target = PathBuf::from(format!("{}.rdb", source.display()));
    let _ = std::fs::remove_file(&target);
    let database = Database::import(&source).expect("the fixture imports");
    let _ = database
        .session()
        .execute_batch("PRAGMA busy_timeout = 5000");
    let path = database.path().to_path_buf();
    cache.insert(name.to_string(), path.clone());
    path
}

/// Opens a connection onto a fixture, importing it if this is the first test
/// in the binary to ask for this name.
fn connect(name: &str) -> Database {
    let database = Database::open(imported_path(name)).expect("the import opens");
    let _ = database
        .session()
        .execute_batch("PRAGMA busy_timeout = 5000");
    database
}

/// Renders a row the way the tests compare it: a tagged string per value, so a
/// difference in storage class is a difference in the text.
fn render(values: &[OwnedDatum]) -> String {
    values
        .iter()
        .map(|value| match value {
            OwnedDatum::Null => "null".to_string(),
            OwnedDatum::Int(integer) => format!("int:{integer}"),
            OwnedDatum::Real(real) => format!("real:{real:?}"),
            OwnedDatum::Text(text) => {
                format!("text:{}", String::from_utf8_lossy(text))
            }
            OwnedDatum::Blob(blob) => format!("blob:{}", hex(blob)),
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
fn rows(connection: &Connection<'_>, sql: &str) -> Vec<String> {
    connection
        .query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .iter()
        .map(|row| render(row))
        .collect()
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
    let database = connect("basic-p4096-utf8.db");
    let connection = database.session();
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
    let database = connect("basic-p4096-utf8.db");
    let connection = database.session();
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
    let database = connect("basic-p4096-utf8.db");
    let connection = database.session();
    let explained = connection
        .explain("SELECT name FROM people WHERE id = 7")
        .expect("it explains")
        .join("\n");
    assert!(
        explained.contains("USING INTEGER PRIMARY KEY"),
        "{explained}"
    );
    let found = rows(&connection, "SELECT name FROM people WHERE id = 7");
    assert_eq!(found, vec!["text:delta echo"]);
}

/// A rowid range becomes a positioned scan that stops at the upper bound.
#[test]
fn a_rowid_range_stops_at_its_bound() {
    let database = connect("basic-p4096-utf8.db");
    let connection = database.session();
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
    let database = connect("basic-p4096-utf8.db");
    let connection = database.session();
    let explained = connection
        .explain("SELECT id FROM people WHERE name = 'alpha'")
        .expect("it explains")
        .join("\n");
    assert!(explained.contains("INDEX"), "{explained}");
    let found = rows(&connection, "SELECT id FROM people WHERE name = 'alpha'");
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
    let database = connect("basic-p4096-utf8.db");
    let connection = database.session();
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
    let database = connect("basic-p4096-utf8.db");
    let connection = database.session();
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
///
/// **The empty-input queries have to clear the fixture's own `big rowid` row**,
/// `id = 9007199254740993`, or they are not empty at all. The bound here used
/// to be `id > 1000000`, which that row satisfies - so `count(*)` answered `1`
/// and would have answered `9007199254740993` for `sum(id)` rather than the
/// `NULL` an aggregate over no rows gives. The row is bigger than any real
/// `id` this fixture holds on purpose (`a_full_scan_returns_every_row_the_plan_produces`
/// is where it is named), so the bound is moved past it instead of removing
/// the case: an empty aggregate is still worth asserting, it just has to
/// actually be empty.
#[test]
fn aggregates_answer_over_the_whole_table() {
    let database = connect("basic-p4096-utf8.db");
    let connection = database.session();
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
            "SELECT count(*) FROM people WHERE id > 9007199254740993"
        ),
        vec!["int:0"]
    );
    assert_eq!(
        rows(
            &connection,
            "SELECT sum(id) FROM people WHERE id > 9007199254740993"
        ),
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
    let database = connect("basic-p4096-utf8.db");
    let connection = database.session();
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
    let database = connect("basic-p4096-utf8.db");
    let connection = database.session();
    let found = rows(&connection, "SELECT DISTINCT note IS NULL FROM people");
    assert_eq!(found, vec!["int:0", "int:1"]);
}

/// `VALUES` runs with no table at all.
#[test]
fn values_runs_without_a_table() {
    let database = connect("basic-p4096-utf8.db");
    let connection = database.session();
    assert_eq!(
        rows(&connection, "VALUES (1, 'a'), (2, 'b')"),
        vec!["int:1|text:a", "int:2|text:b"]
    );
    assert_eq!(
        rows(&connection, "SELECT 1 + 1, 'x' || 'y', NULL"),
        vec!["int:2|text:xy|null"]
    );
}

/// Parameters bind by index, and rebinding after a reset works.
#[test]
fn parameters_bind_and_rebind() {
    let database = connect("basic-p4096-utf8.db");
    let connection = database.session();
    let mut statement = connection
        .prepare("SELECT name FROM people WHERE id = ?1")
        .expect("it prepares");
    statement.bind_integer(1, 7).expect("it binds");
    assert!(statement.step().expect("it steps"));
    assert_eq!(render(statement.row()), "text:delta echo");
    statement.reset();
    statement.bind_integer(1, 1).expect("it rebinds");
    assert!(statement.step().expect("it steps"));
    assert_eq!(render(statement.row()), "text:alpha");
}

/// Result metadata names each column.
///
/// The old engine also reported a result column's table/database origin and
/// its declared type; `Statement::columns()` on the new engine answers only
/// the name each column is reported under, because there is no public
/// per-statement accessor for the rest. What survives is the part every
/// caller actually reads a result set by: the name.
///
/// **Stepped before the columns are read.** On this engine `columns()` is
/// empty until the statement has produced a row - `Statement::columns`'s own
/// doc comment says so, `tests/lifecycle.rs`'s
/// `column_metadata_is_available_after_stepping` pins it, and this test used
/// to read the names straight after `prepare` the way `sqlite3_column_name`
/// allows, which asks a question this engine does not answer until later.
#[test]
fn result_metadata_names_the_column() {
    let database = connect("basic-p4096-utf8.db");
    let connection = database.session();
    let mut statement = connection
        .prepare("SELECT name, score AS s, id + 1 FROM people")
        .expect("it prepares");
    assert!(statement.step().expect("it steps"), "people has rows");
    // An unaliased expression is named after the text it was written as,
    // which is SQLite's default and what `sqlite3_column_name` reports.
    assert_eq!(
        statement.columns().to_vec(),
        vec!["name".to_string(), "s".to_string(), "id + 1".to_string()]
    );
}

/// A name nobody declared is an error at prepare time, with an offset.
#[test]
fn an_unknown_name_fails_at_prepare_with_an_offset() {
    let database = connect("basic-p4096-utf8.db");
    let connection = database.session();
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
///
/// The import writes `<fixture>.rdb` beside the source and reads only that; a
/// promise this engine could break by opening a SQLite file directly is not
/// one it can break by construction, so what this still proves is that
/// opening the import and querying it - whether that is this fixture's first
/// import in the binary or a cache hit from an earlier test - never touches
/// the source `.db`.
#[test]
fn reading_changes_no_byte_of_the_file() {
    for fixture_row in valid_fixtures() {
        let path = fixture(fixture_row.name);
        if !path.is_file() {
            continue;
        }
        let before = std::fs::read(&path).expect("the fixture reads");
        {
            let database = connect(fixture_row.name);
            let connection = database.session();
            // Whatever the fixture holds, running one query over it is enough
            // to touch the pager, the cache and a cursor.
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
        let database = connect(name);
        let connection = database.session();
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
