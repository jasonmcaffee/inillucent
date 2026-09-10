//! The catalog: what it reads, what it refuses, and what invalidates it.
//!
//! Invariant: a schema that cannot be understood is reported as corruption
//! naming the object, never as a table with no columns. The difference matters:
//! a table with no columns makes every query against it fail with "no such
//! column", which sends the reader looking at their SQL instead of at the file.
//!
//! Prepared-statement invalidation is tested by actually moving the schema
//! under a prepared statement, with the pinned SQLite binary doing the moving.
//! A test that bumped the cookie itself would be testing the test.

use std::path::{Path, PathBuf};

use inillucent_catalog::table_from_create_sql;
use inillucent_compat::oracle::{Driver, Op};
use inillucent_compat::workspace_root;
use inillucent_legacy::Database;
use inillucent_sql::catalog_view::{CatalogView, TableKind};

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

/// Runs statements against an existing database with the pinned binary.
fn mutate(path: &Path, statements: &[&str]) -> bool {
    let Some(program) = sqlite_oracle() else {
        return false;
    };
    let Ok(mut driver) = Driver::start("sqlite", &program) else {
        return false;
    };
    if driver.send(&Op::Hello).is_err()
        || driver.send(&Op::Open(path.display().to_string())).is_err()
    {
        return false;
    }
    for statement in statements {
        let Ok(observation) = driver.send(&Op::Exec((*statement).to_string())) else {
            return false;
        };
        assert!(observation.ok, "{statement}: {}", observation.message);
    }
    let _ = driver.send(&Op::Bye);
    true
}

/// Opens a database with a busy timeout, because these tests run in parallel.
fn open(path: &Path) -> Database {
    Database::open_with_busy_timeout(path, std::time::Duration::from_secs(5))
        .expect("the database opens")
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
/// to the table they index and their root pages filled in.
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
        eprintln!("the pinned SQLite oracle is not built; skipping");
        return;
    };
    let database = open(&path);
    let connection = database.connect().expect("it connects");
    let catalog = connection.catalog().expect("the catalog is readable");

    let table = catalog.find_table(None, b"t").expect("t resolves");
    assert_eq!(table.rowid_alias, Some(0));
    assert_eq!(
        table.columns.get(1).map(|column| column.collation.clone()),
        Some(b"nocase".to_vec())
    );
    // Two created indexes, and every one of them has a real root page.
    let created: Vec<&inillucent_sql::catalog_view::IndexInfo> = table
        .indexes
        .iter()
        .filter(|index| index.origin == inillucent_sql::catalog_view::IndexOrigin::Created)
        .collect();
    assert_eq!(created.len(), 2);
    for index in &table.indexes {
        assert!(index.root > 0, "{:?} has no root page", index.name);
    }

    let without = catalog.find_table(None, b"u").expect("u resolves");
    assert!(without.without_rowid);
    assert_eq!(without.rowid_alias, None);
    // The automatic index for a WITHOUT ROWID primary key is the table itself,
    // so SQLite writes no `sqlite_autoindex` row for it.
    assert!(without
        .indexes
        .iter()
        .all(|index| index.origin != inillucent_sql::catalog_view::IndexOrigin::Created));

    let view = catalog.find_table(None, b"v").expect("v resolves");
    assert_eq!(view.kind, TableKind::View);
}

/// An automatic index gets its root page from the schema row even though the
/// row carries no SQL for it.
#[test]
fn an_automatic_index_gets_its_root_page() {
    let Some(path) = build(
        "autoindex.db",
        &["CREATE TABLE t (a TEXT UNIQUE, b TEXT, UNIQUE (b, a))"],
    ) else {
        eprintln!("the pinned SQLite oracle is not built; skipping");
        return;
    };
    let database = open(&path);
    let connection = database.connect().expect("it connects");
    let catalog = connection.catalog().expect("the catalog is readable");
    let table = catalog.find_table(None, b"t").expect("t resolves");
    assert_eq!(table.indexes.len(), 2);
    for index in &table.indexes {
        assert!(
            index.root > 0,
            "{} has no root page",
            String::from_utf8_lossy(&index.name)
        );
        assert!(index.unique);
    }
    // And the reconstructed keys are in declaration order, so an index seek
    // built from them probes the right columns.
    assert_eq!(
        table.indexes.first().map(|index| index.columns.len()),
        Some(1)
    );
    assert_eq!(
        table.indexes.get(1).map(|index| index.columns.len()),
        Some(2)
    );
}

/// A schema change under a prepared statement recompiles it.
///
/// The pinned binary does the changing, so this is the real sequence: prepare,
/// let another process alter the table, reload, step. `prepare_v2` recompiles
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
        eprintln!("the pinned SQLite oracle is not built; skipping");
        return;
    };
    let database = open(&path);
    let connection = database.connect().expect("it connects");
    let cookie_before = connection.schema_cookie(0).expect("the cookie reads");

    let mut statement = connection.prepare("SELECT * FROM t").expect("it prepares");
    assert_eq!(statement.column_count(), 2);

    // Another process adds a column. The prepared statement has not run yet.
    assert!(mutate(
        &path,
        &["ALTER TABLE t ADD COLUMN c TEXT DEFAULT 'x'"]
    ));
    connection.reload_schema().expect("the schema reloads");
    let cookie_after = connection.schema_cookie(0).expect("the cookie reads");
    assert_ne!(
        cookie_before, cookie_after,
        "the schema cookie must move when the schema does"
    );

    // Stepping recompiles, so `*` now expands to three columns.
    assert!(statement.step().expect("it steps"));
    assert_eq!(statement.column_count(), 3);
    assert_eq!(statement.value_integer(0), Some(1));
    assert_eq!(statement.value_text(1), Some("one".to_string()));
    // The existing row was not rewritten by the ALTER, so its record stops
    // before the new column - and SQLite reads the column's DEFAULT back for
    // exactly those rows rather than NULL. Verified against the pinned build:
    // `typeof(c), quote(c)` answers `text|'x'`.
    assert_eq!(statement.value_text(2), Some("x".to_string()));
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
        eprintln!("the pinned SQLite oracle is not built; skipping");
        return;
    };
    let database = open(&path);
    let connection = database.connect().expect("it connects");
    let mut statement = connection.prepare("SELECT a FROM t").expect("it prepares");

    assert!(mutate(&path, &["DROP TABLE t"]));
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
        eprintln!("the pinned SQLite oracle is not built; skipping");
        return;
    };
    let database = open(&path);
    let connection = database.connect().expect("it connects");
    let mut statement = connection.prepare("SELECT a FROM t").expect("it prepares");
    let before = statement.explain();
    assert!(statement.step().expect("it steps"));
    statement.reset().expect("it resets");
    assert!(statement.step().expect("it steps"));
    assert_eq!(statement.explain(), before);
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
/// statement, so that is what the failure says.
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
        eprintln!("the pinned SQLite oracle is not built; skipping");
        return;
    };
    let database = open(&path);
    let failure = match database.connect() {
        Err(failure) => failure,
        Ok(_) => panic!("a schema that will not parse must not connect"),
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
