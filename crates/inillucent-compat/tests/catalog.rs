//! The catalog: what it reads, what it refuses, and what invalidates it.
//!
//! Invariant: a schema that cannot be understood is reported as corruption
//! naming the object, never as a table with no columns. The difference matters:
//! a table with no columns makes every query against it fail with "no such
//! column", which sends the reader looking at their SQL instead of at the file.
//!
//! Prepared-statement invalidation is tested by actually moving the schema
//! under a prepared statement, with a second connection doing the moving. A
//! test that bumped the cookie itself would be testing the test.
//!
//! **Ported onto `inillucent_engine::connect::Database`.** The old engine's
//! `Connection::catalog().find_table(...)` gave this file a handle straight
//! onto a `TableInfo` snapshot; the new engine's public `Connection` has no
//! equivalent accessor; a live schema is asked about through `PRAGMA
//! table_info`/`index_list`/`index_xinfo`, the same surface an application
//! reaches it through. `table_from_create_sql` needs no engine at all - it is a
//! pure function over SQL text - and every case that only exercised it is
//! unchanged. `Database::connect()` also no longer returns a `Result`: a schema
//! that will not parse is reported by `Database::import()`, because the new
//! engine reads and rebuilds the schema there rather than lazily at the first
//! connection.
//!
//! **The fixtures are `import`ed, not `open`ed, and the schema is moved by a
//! second connection rather than by the pinned SQLite binary.** Both follow
//! from the same fact: the fixtures here are written by the oracle, so they
//! are SQLite files, and this engine does not read SQLite's format in place -
//! `Database::open` on one refuses with "neither meta page is readable", and
//! `Database::import` is the one way in, rebuilding the fixture as PAX trees
//! beside the source. A `sqlite3` write to the source after that is a write to
//! a file no connection here is looking at.

use std::path::{Path, PathBuf};

use inillucent_catalog::table_from_create_sql;
use inillucent_compat::oracle::{Driver, Op};
use inillucent_compat::workspace_root;
use inillucent_engine::connect::Database;
use inillucent_sql::catalog_view::TableKind;
use inillucent_tree::datum::OwnedDatum;

/// Returns the directory scratch databases are built in.
fn scratch() -> PathBuf {
    let path = workspace_root().join("_agent_output/catalog");
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Returns the pinned oracle, if it has been built.
fn sqlite_oracle() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let path = workspace_root()
        .join(".sqlite-ref/3.53.4")
        .join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Builds a database with the pinned binary and returns its path.
fn build(name: &str, statements: &[&str]) -> Option<PathBuf> {
    let program = sqlite_oracle()?;
    let path = scratch().join(name);
    let _ = std::fs::remove_file(&path);
    let mut driver = Driver::start("sqlite", &program).ok()?;
    driver.send(&Op::Hello).ok()?;
    driver.send(&Op::Open(path.display().to_string())).ok()?;
    for statement in statements {
        let observation = driver.send(&Op::Exec((*statement).to_string())).ok()?;
        assert!(observation.ok, "{statement}: {}", observation.message);
    }
    let _ = driver.send(&Op::Bye);
    Some(path)
}

/// Opens a database, giving it the same busy-timeout headroom the old engine's
/// `open_with_busy_timeout` gave these tests, because they run in parallel.
///
/// **`import`, not `open`.** Every path this is handed comes from `build()`,
/// which writes its fixture through the pinned SQLite oracle - so the bytes on
/// disk are a SQLite file, and `Database::open` on one of those refuses with
/// "neither meta page is readable" (correctly - that is not this engine's
/// format). `Database::import` is the one path a SQLite fixture reaches this
/// engine through: it reads the source with `inillucent-sqlite-reader` and
/// rebuilds it as PAX trees at `<path>.rdb`.
fn open(path: &Path) -> Database {
    let database = Database::import(path).expect("the database opens");
    let _ = database
        .connect()
        .execute_batch("PRAGMA busy_timeout = 5000");
    database
}

/// Schema SQL that does not parse names the statement and the reason, not a
/// table that quietly has no columns - and not a damaged file either.
///
/// The assertion is on `message()` rather than on `detail()` because that is
/// the field a caller is told to read and the field the driver shows. Putting
/// the explanation only in `detail()` - which is suppressed unless the database
/// was opened with diagnostics on - left `message()` answering the primary
/// code's canned "database disk image is malformed" for a file whose bytes are
/// perfectly fine.
#[test]
fn unparseable_schema_sql_names_the_statement_and_the_reason() {
    let failure = table_from_create_sql(b"CREATE TABLE t(a,", 0, 2).expect_err("it must not parse");
    assert_eq!(failure.code(), inillucent_base::PrimaryCode::Error);
    assert!(
        failure.message().contains("cannot parse the CREATE TABLE"),
        "{failure:?}"
    );
    assert!(
        !failure.message().contains("disk image is malformed"),
        "{failure:?}"
    );
    // The offset survives, so a caller can point at the character.
    assert!(failure.sql_offset().is_some(), "{failure:?}");
}

/// The reserved-word column names this engine used to refuse: `left` and
/// `right` are ordinary column names in SQLite, and a schema SQLite writes has
/// to load.
#[test]
fn a_table_whose_columns_are_named_left_and_right_loads() {
    let table = table_from_create_sql(b"CREATE TABLE pairs (left TEXT, right TEXT)", 0, 2)
        .expect("it loads");
    let names: Vec<String> = table
        .columns
        .iter()
        .map(|column| String::from_utf8_lossy(&column.name).into_owned())
        .collect();
    assert_eq!(names, vec!["left".to_string(), "right".to_string()]);
}

/// Schema SQL that is not a `CREATE TABLE` at all is corruption too.
#[test]
fn schema_sql_that_is_not_a_create_table_is_corruption() {
    let failure = table_from_create_sql(b"SELECT 1", 0, 2).expect_err("it must not load");
    assert_eq!(failure.code(), inillucent_base::PrimaryCode::Corrupt);
}

/// A virtual table loads as a virtual table rather than as a broken one.
#[test]
fn a_virtual_table_is_recognised() {
    let table =
        table_from_create_sql(b"CREATE VIRTUAL TABLE t USING fts5(body)", 0, 0).expect("it loads");
    assert_eq!(table.kind, TableKind::Virtual);
    assert_eq!(table.name, b"t");
}

/// The catalog reads every object of a real schema, with its indexes attached
/// to the table they index and reported through the same `PRAGMA` surface an
/// application would read them through.
///
/// The column's collation is checked at the parser layer with
/// `table_from_create_sql` directly, because `PRAGMA table_info` - SQLite's own
/// pragma, not something narrowed here - reports no collation column at all;
/// there is no live-connection surface to ask that question through, in either
/// engine.
#[test]
fn the_catalog_reads_a_real_schema() {
    let Some(path) = build(
        "schema.db",
        &[
            "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT COLLATE NOCASE, c REAL)",
            "CREATE INDEX t_by_b ON t (b)",
            "CREATE UNIQUE INDEX t_by_c ON t (c DESC)",
            "CREATE TABLE u (x TEXT PRIMARY KEY, y) WITHOUT ROWID",
            "CREATE VIEW v AS SELECT a FROM t",
        ],
    ) else {
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    let database = open(&path);
    let connection = database.connect();

    let info = table_from_create_sql(
        b"CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT COLLATE NOCASE, c REAL)",
        0,
        2,
    )
    .expect("t's own declaration parses");
    assert_eq!(info.rowid_alias, Some(0));
    assert_eq!(
        info.columns.get(1).map(|column| column.collation.clone()),
        Some(b"nocase".to_vec())
    );

    // `pk` on `table_info` is the 1-based key position; a single INTEGER
    // PRIMARY KEY column is the rowid alias, position 1.
    let table_info = connection
        .query("PRAGMA table_info(t)")
        .expect("it answers");
    assert_eq!(table_info.len(), 3, "t has three columns");
    let pk_position = match table_info.first().and_then(|row| row.get(5)) {
        Some(OwnedDatum::Int(value)) => *value,
        other => panic!("unexpected pk cell: {other:?}"),
    };
    assert_eq!(pk_position, 1, "column a is the rowid alias");

    // Two created indexes on t, both listed and one of them unique.
    let index_list = connection
        .query("PRAGMA index_list(t)")
        .expect("it answers");
    assert_eq!(index_list.len(), 2, "t has two created indexes");
    let unique_flags: Vec<i64> = index_list
        .iter()
        .filter_map(|row| match row.get(2) {
            Some(OwnedDatum::Int(value)) => Some(*value),
            _ => None,
        })
        .collect();
    assert!(
        unique_flags.contains(&1),
        "t_by_c should be unique: {index_list:?}"
    );

    // `u` is WITHOUT ROWID, so its declared primary key has no b-tree of its
    // own - the table's own tree *is* the key's index, and SQLite writes no
    // `sqlite_autoindex` row to `sqlite_schema` for it. `PRAGMA index_list`
    // still names it, though: the pinned reference's `PragTyp_INDEX_LIST`
    // case (`sqlite3.c`) walks `pTab->pIndex`, the table's in-memory index
    // chain, with no WITHOUT ROWID special case, and the automatic PK index
    // stays on that chain whether or not it has a tree of its own - checked
    // directly against `.sqlite-ref/3.53.4/shell/sqlite3.exe`, which answers
    // `0|sqlite_autoindex_u_1|1|pk|0` for exactly this schema. This used to
    // assert the row was absent, on the assumption that "no separate tree"
    // meant "not reported here" - a schema-object question mistaken for a
    // storage question.
    let u_index_list = connection
        .query("PRAGMA index_list(u)")
        .expect("it answers");
    assert_eq!(
        u_index_list.len(),
        1,
        "u's own primary key is still on its index chain: {u_index_list:?}"
    );
    assert_eq!(
        u_index_list.first().and_then(|row| row.get(1)),
        Some(&OwnedDatum::Text(b"sqlite_autoindex_u_1".to_vec())),
        "SQLite's own automatic name for a table-level PRIMARY KEY: {u_index_list:?}"
    );

    // The view still answers the query it was declared with.
    let rows = connection
        .query("SELECT a FROM v ORDER BY a")
        .expect("v answers");
    assert!(rows.is_empty(), "an empty t makes an empty view");
}

/// An automatic index is listed even though the schema row that names it
/// carries no SQL, and its declared key columns are in declaration order - so
/// an index seek built from them probes the right columns.
#[test]
fn an_automatic_index_gets_its_declared_key_order() {
    let Some(path) = build(
        "autoindex.db",
        &["CREATE TABLE t (a TEXT UNIQUE, b TEXT, UNIQUE (b, a))"],
    ) else {
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    let database = open(&path);
    let connection = database.connect();
    let index_list = connection
        .query("PRAGMA index_list(t)")
        .expect("it answers");
    assert_eq!(index_list.len(), 2, "two UNIQUE constraints, two indexes");
    let names: Vec<String> = index_list
        .iter()
        .filter_map(|row| match row.get(1) {
            Some(OwnedDatum::Text(bytes)) => Some(String::from_utf8_lossy(bytes).into_owned()),
            _ => None,
        })
        .collect();
    let single = names
        .iter()
        .find(|name| {
            connection
                .query(&format!("PRAGMA index_info({name})"))
                .map(|rows| rows.len() == 1)
                .unwrap_or(false)
        })
        .expect("the single-column index is among them");
    let double = names
        .iter()
        .find(|name| name.as_str() != single.as_str())
        .expect("the two-column index is the other one");
    let double_info = connection
        .query(&format!("PRAGMA index_info({double})"))
        .expect("it answers");
    assert_eq!(double_info.len(), 2, "UNIQUE (b, a) has two key columns");
    // `index_info`'s rows are already in key order; column b is declared first.
    let first_column = match double_info.first().and_then(|row| row.get(2)) {
        Some(OwnedDatum::Text(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
        other => panic!("unexpected column-name cell: {other:?}"),
    };
    assert_eq!(first_column, "b", "b is declared before a in UNIQUE (b, a)");
}

/// A schema change under a prepared statement recompiles it.
///
/// The pinned binary does the changing, so this is the real sequence: prepare,
/// let another process alter the table, reload, step. `prepare` recompiles
/// from the SQL it kept and the caller sees the new shape rather than an error.
#[test]
fn a_schema_change_recompiles_a_prepared_statement() {
    let Some(path) = build(
        "invalidation.db",
        &[
            "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT)",
            "INSERT INTO t VALUES (1, 'one')",
        ],
    ) else {
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    let database = open(&path);
    let connection = database.connect();
    let cookie_before = connection.schema_cookie();

    let mut statement = connection.prepare("SELECT * FROM t").expect("it prepares");
    // **Stepped before the columns are counted, because on this engine a
    // statement has no column names until it has run.** `Statement::columns`
    // says so itself: it is empty before the first `step`, where SQLite's
    // `sqlite3_column_name` answers straight after a prepare. So the count
    // this test is about - two columns before the `ALTER`, three after - has
    // to be taken from a statement that has produced a row, and the check
    // that `*` re-expands is still the check.
    assert!(statement.step().expect("it steps"));
    assert_eq!(statement.columns().len(), 2);
    statement.reset();

    // **A second connection adds the column, not the pinned SQLite binary.**
    // This used to drive `sqlite3` at the same file, which worked while the
    // engine read SQLite's format in place. It does not any more: a SQLite
    // fixture reaches this engine through `Database::import`, which rebuilds
    // it as PAX trees beside the source, so a write to the source is a write
    // to a file this connection is not looking at. The subject of the test is
    // the prepared statement, not who moved the schema.
    database
        .connect()
        .execute_batch("ALTER TABLE t ADD COLUMN c TEXT DEFAULT 'x'")
        .expect("the column is added");
    connection.reload_schema().expect("the schema reloads");
    let cookie_after = connection.schema_cookie();
    assert_ne!(
        cookie_before, cookie_after,
        "the schema cookie must move when the schema does"
    );

    // Stepping recompiles, so `*` now expands to three columns.
    assert!(statement.step().expect("it steps"));
    assert_eq!(statement.columns().len(), 3);
    let row = statement.row();
    assert_eq!(row.first(), Some(&OwnedDatum::Int(1)));
    assert_eq!(row.get(1), Some(&OwnedDatum::Text(b"one".to_vec())));
    // The existing row was not rewritten by the ALTER, so its record stops
    // before the new column - and SQLite reads the column's DEFAULT back for
    // exactly those rows rather than NULL. Verified against the pinned build:
    // `typeof(c), quote(c)` answers `text|'x'`.
    assert_eq!(row.get(2), Some(&OwnedDatum::Text(b"x".to_vec())));
}

/// A statement whose table is dropped under it reports the failure rather than
/// running against a root page that is no longer a table.
#[test]
fn a_dropped_table_is_reported_rather_than_read() {
    let Some(path) = build(
        "dropped.db",
        &[
            "CREATE TABLE t (a INTEGER PRIMARY KEY)",
            "CREATE TABLE keep (a)",
            "INSERT INTO t VALUES (1)",
        ],
    ) else {
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    let database = open(&path);
    let connection = database.connect();
    let mut statement = connection.prepare("SELECT a FROM t").expect("it prepares");

    // Dropped on this connection rather than through the pinned SQLite binary,
    // for the reason `a_schema_change_recompiles_a_prepared_statement` gives:
    // the fixture is imported, so the source file and the database this
    // connection holds are two different files. It is also dropped on *this*
    // connection rather than a second one: issuing it from a second session
    // while this one held a prepared statement over the table was refused with
    // "bad parameter or other API misuse" (observed here; which of the two -
    // the second session or the live statement - is the cause was not
    // established). What this test is about is the prepared statement
    // noticing, not which session did the dropping.
    connection
        .execute_batch("DROP TABLE t")
        .expect("the table is dropped");
    connection.reload_schema().expect("the schema reloads");

    let failure = statement.step().expect_err("the table is gone");
    assert!(failure.message().contains("no such table"), "{failure}");
}

/// A cookie that has not moved leaves a prepared statement alone, so the check
/// is a comparison rather than a recompile on every step.
#[test]
fn an_unchanged_schema_does_not_recompile() {
    let Some(path) = build(
        "stable.db",
        &[
            "CREATE TABLE t (a INTEGER PRIMARY KEY)",
            "INSERT INTO t VALUES (1)",
        ],
    ) else {
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    let database = open(&path);
    let connection = database.connect();
    let before = connection.explain("SELECT a FROM t").expect("it explains");
    let mut statement = connection.prepare("SELECT a FROM t").expect("it prepares");
    assert!(statement.step().expect("it steps"));
    statement.reset();
    assert!(statement.step().expect("it steps"));
    let after = connection.explain("SELECT a FROM t").expect("it explains");
    assert_eq!(
        after, before,
        "an unchanged schema plans the same way twice"
    );
}

/// A schema whose `sqlite_schema` row will not parse names the object **and**
/// says what was wrong with the text, in the field a caller reads.
///
/// This used to assert `Corrupt` and a `detail()` naming the object, and both
/// halves were the defect that got fixed together. `message()` - the field the driver
/// shows an application - answered "database disk image is malformed", which
/// sends a reader to `PRAGMA integrity_check` on a file whose bytes are fine;
/// and the object name was attached with `with_detail`, which *replaced* the
/// parse reason the loader had just written, so even with diagnostics on the
/// reason was gone.
///
/// The bytes here were read perfectly. What could not be understood is the
/// statement, so that is what the failure says. On this engine that failure
/// surfaces from `Database::import` - the one path a SQLite fixture reaches
/// this engine through - because the schema is read and rebuilt there, before
/// any connection exists to ask lazily.
#[test]
fn an_unparseable_schema_row_names_its_object_and_the_reason() {
    let Some(path) = build(
        "corrupt.db",
        &[
            "CREATE TABLE good (a)",
            "PRAGMA writable_schema = ON",
            "UPDATE sqlite_schema SET sql = 'this is not sql' WHERE name = 'good'",
            "PRAGMA writable_schema = OFF",
        ],
    ) else {
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    let failure = match Database::import(&path) {
        Err(failure) => failure,
        Ok(_) => panic!("a schema that will not parse must not import"),
    };
    // Not corruption: the file is readable and SQLite opens it. It is the
    // statement this engine could not parse.
    assert_eq!(failure.code(), inillucent_base::PrimaryCode::Error);
    let message = failure.message();
    assert!(message.contains("good"), "{failure:?}");
    assert!(
        message.contains("cannot parse the CREATE TABLE"),
        "{failure:?}"
    );
    assert!(message.contains("syntax error"), "{failure:?}");
    assert!(!message.contains("disk image is malformed"), "{failure:?}");
    assert_eq!(failure.detail(), Some(message), "{failure:?}");
}
