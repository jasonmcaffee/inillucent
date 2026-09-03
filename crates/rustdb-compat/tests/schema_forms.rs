//! The schema surface: views, `STRICT`, and the objects a `DROP` takes with it.
//!
//! Invariant: every fixture here is built by rust-db and then opened, read and
//! `PRAGMA integrity_check`ed by the pinned SQLite 3.53.4 binary, and the same
//! statements are run against both engines. A schema form rust-db can write but
//! SQLite cannot read is a parity failure that no single-engine test can see.

use std::path::PathBuf;

use rustdb::{Database, Value};
use rustdb_compat::oracle::{Driver, Op, TaggedValue};
use rustdb_compat::workspace_root;

/// Returns the pinned oracle binary, when it has been built.
fn oracle_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("RUSTDB_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let path = workspace_root()
        .join(".sqlite-ref/3.53.4")
        .join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Returns a scratch path nothing else in this file uses.
fn scratch(tag: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("schema-forms");
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join(format!("{tag}.db"));
    let _ = std::fs::remove_file(&path);
    path
}

/// Renders one value as a tagged string.
fn render(value: &Value<'static>) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Integer(integer) => format!("int:{integer}"),
        Value::Real(real) => format!("real:{real:?}"),
        Value::Text(text) => format!("text:{}", String::from_utf8_lossy(&text.utf8_bytes())),
        Value::Blob(blob) => format!(
            "blob:{}",
            blob.raw()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
    }
}

/// Renders one of the oracle's tagged values the same way.
fn render_tagged(value: &TaggedValue) -> String {
    match value {
        TaggedValue::Null => "null".to_string(),
        TaggedValue::Integer(integer) => format!("int:{integer}"),
        TaggedValue::Real(real) => format!("real:{real:?}"),
        TaggedValue::Text(bytes) => format!("text:{}", String::from_utf8_lossy(bytes)),
        TaggedValue::Blob(bytes) => format!(
            "blob:{}",
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
    }
}

/// Runs a statement through rust-db, returning its rows or its failure.
fn run(connection: &rustdb::Connection, sql: &str) -> Result<Vec<String>, String> {
    let mut statement = match connection.prepare(sql) {
        Ok(statement) => statement,
        Err(reason) => return Err(reason.message().to_string()),
    };
    let mut rows = Vec::new();
    loop {
        match statement.step() {
            Ok(true) => rows.push(
                statement
                    .row()
                    .iter()
                    .map(render)
                    .collect::<Vec<String>>()
                    .join("|"),
            ),
            Ok(false) => break,
            Err(reason) => return Err(reason.message().to_string()),
        }
    }
    Ok(rows)
}

/// Runs a script through rust-db, asserting every statement succeeds.
fn run_all(connection: &rustdb::Connection, script: &[&str]) {
    for sql in script {
        run(connection, sql).unwrap_or_else(|reason| panic!("{sql}: {reason}"));
    }
}

/// Opens a database rust-db wrote with the pinned binary and checks it over.
///
/// The integrity check is the point: a schema row rust-db writes with the wrong
/// shape produces a file SQLite opens and then reports as corrupt, and a test
/// that only re-read the file with rust-db would agree with itself.
fn sqlite_reads(path: &PathBuf, queries: &[(&str, &[&str])]) {
    let Some(program) = oracle_path() else {
        panic!("the pinned SQLite oracle is not built");
    };
    let mut driver = Driver::start("sqlite", &program).expect("the oracle starts");
    driver.send(&Op::Hello).expect("the oracle answers");
    driver
        .send(&Op::Open(path.display().to_string()))
        .expect("the oracle opens the file");
    let integrity = driver
        .send(&Op::Query("PRAGMA integrity_check".to_string()))
        .expect("the oracle answers");
    let reported: Vec<String> = integrity
        .rows
        .iter()
        .flat_map(|row| row.iter().map(render_tagged))
        .collect();
    assert_eq!(reported, vec!["text:ok".to_string()], "integrity_check");
    for (sql, expected) in queries {
        let observation = driver
            .send(&Op::Query((*sql).to_string()))
            .expect("the oracle answers");
        assert!(observation.ok, "{sql}: {}", observation.message);
        let rows: Vec<String> = observation
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(render_tagged)
                    .collect::<Vec<String>>()
                    .join("|")
            })
            .collect();
        assert_eq!(rows, expected.to_vec(), "{sql}");
    }
}

/// A view rust-db creates is one SQLite reads, and it selects the same rows.
#[test]
fn a_view_round_trips_through_sqlite() {
    let path = scratch("views");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)",
            "INSERT INTO t VALUES (1, 'ada', 10.5)",
            "INSERT INTO t VALUES (2, 'bob', -2.0)",
            "INSERT INTO t VALUES (3, 'cai', 99.0)",
            "CREATE VIEW high AS SELECT id, name FROM t WHERE score > 0",
            "CREATE VIEW named (who) AS SELECT name FROM t",
        ],
    );
    assert_eq!(
        run(&connection, "SELECT * FROM high ORDER BY id"),
        Ok(vec![
            "int:1|text:ada".to_string(),
            "int:3|text:cai".to_string()
        ])
    );
    assert_eq!(
        run(&connection, "SELECT who FROM named ORDER BY who"),
        Ok(vec![
            "text:ada".to_string(),
            "text:bob".to_string(),
            "text:cai".to_string()
        ])
    );
    // A view is not a table, in both directions.
    assert!(run(&connection, "INSERT INTO high VALUES (4, 'dee')").is_err());
    assert!(run(&connection, "DROP TABLE high").is_err());
    assert!(run(&connection, "DROP VIEW t").is_err());
    drop(connection);
    drop(database);

    sqlite_reads(
        &path,
        &[
            (
                "SELECT type, name FROM sqlite_schema WHERE type = 'view' ORDER BY name",
                &["text:view|text:high", "text:view|text:named"],
            ),
            (
                "SELECT * FROM high ORDER BY id",
                &["int:1|text:ada", "int:3|text:cai"],
            ),
            (
                "SELECT who FROM named ORDER BY who",
                &["text:ada", "text:bob", "text:cai"],
            ),
        ],
    );
}

/// Dropping a view removes its row and nothing else.
#[test]
fn dropping_a_view_leaves_the_table() {
    let path = scratch("drop-view");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
            "INSERT INTO t VALUES (1, 'ada')",
            "CREATE VIEW v AS SELECT name FROM t",
            "DROP VIEW v",
        ],
    );
    assert!(run(&connection, "SELECT * FROM v").is_err());
    assert!(run(&connection, "DROP VIEW v").is_err());
    run_all(&connection, &["DROP VIEW IF EXISTS v"]);
    assert_eq!(
        run(&connection, "SELECT name FROM t"),
        Ok(vec!["text:ada".to_string()])
    );
    drop(connection);
    drop(database);
    sqlite_reads(
        &path,
        &[
            (
                "SELECT count(*) FROM sqlite_schema WHERE type = 'view'",
                &["int:0"],
            ),
            ("SELECT name FROM t", &["text:ada"]),
        ],
    );
}

/// A `STRICT` table refuses a value whose class its column does not declare,
/// and accepts one the affinity converts.
#[test]
fn strict_tables_refuse_the_wrong_class() {
    let path = scratch("strict");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    run_all(
        &connection,
        &["CREATE TABLE s (a INT, b TEXT, c REAL, d BLOB, e ANY) STRICT"],
    );
    // A declared type outside the six is refused at creation.
    assert!(run(&connection, "CREATE TABLE bad (a VARCHAR(10)) STRICT").is_err());
    assert!(run(&connection, "CREATE TABLE bad (a) STRICT").is_err());

    run_all(
        &connection,
        &[
            "INSERT INTO s VALUES (1, 'x', 1.5, x'00', 'anything')",
            // Affinity runs first, so text that is an integer is stored as one,
            // an integer written to a TEXT column becomes text, and a real
            // whose value is exactly an integer is accepted by an INT column.
            "INSERT INTO s VALUES ('2', 5, 2, x'01', 7)",
            "INSERT INTO s VALUES (2.0, 'z', 1.5, x'02', x'03')",
            "INSERT INTO s VALUES (NULL, NULL, NULL, NULL, NULL)",
        ],
    );
    // What is refused, and what is not, is the pinned binary's own behaviour,
    // probed rather than assumed: STRICT applies the column's affinity first
    // and then checks the class, so an integer written to a TEXT column becomes
    // text and is accepted, while a blob written to the same column is not.
    let refused = [
        "INSERT INTO s VALUES ('abc', 'x', 1.0, x'00', 1)",
        "INSERT INTO s VALUES (1.5, 'x', 1.0, x'00', 1)",
        "INSERT INTO s VALUES (1, x'00ff', 1.0, x'00', 1)",
        "INSERT INTO s VALUES (1, 'x', 1.0, 'notablob', 1)",
    ];
    for sql in refused {
        let outcome = run(&connection, sql);
        assert!(outcome.is_err(), "{sql} was accepted");
        let message = outcome.err().unwrap_or_default();
        assert!(
            message.contains("cannot store"),
            "{sql}: unexpected message {message}"
        );
    }
    assert_eq!(
        run(
            &connection,
            "SELECT typeof(a), typeof(b), typeof(c), typeof(e) FROM s ORDER BY rowid"
        ),
        Ok(vec![
            "text:integer|text:text|text:real|text:text".to_string(),
            "text:integer|text:text|text:real|text:integer".to_string(),
            "text:integer|text:text|text:real|text:blob".to_string(),
            "text:null|text:null|text:null|text:null".to_string(),
        ])
    );
    drop(connection);
    drop(database);
    sqlite_reads(
        &path,
        &[
            (
                "SELECT typeof(a), typeof(b), typeof(c), typeof(e) FROM s ORDER BY rowid",
                &[
                    "text:integer|text:text|text:real|text:text",
                    "text:integer|text:text|text:real|text:integer",
                    "text:integer|text:text|text:real|text:blob",
                    "text:null|text:null|text:null|text:null",
                ],
            ),
            (
                "SELECT count(*) FROM sqlite_schema WHERE sql LIKE '%STRICT%'",
                &["int:1"],
            ),
        ],
    );
}

/// A file SQLite wrote with a `STRICT` table is one rust-db enforces too.
#[test]
fn strict_is_enforced_on_a_file_sqlite_wrote() {
    let path = scratch("strict-from-sqlite");
    let Some(program) = oracle_path() else {
        panic!("the pinned SQLite oracle is not built");
    };
    let mut driver = Driver::start("sqlite", &program).expect("the oracle starts");
    driver.send(&Op::Hello).expect("the oracle answers");
    driver
        .send(&Op::Open(path.display().to_string()))
        .expect("the oracle opens the file");
    for sql in [
        "CREATE TABLE s (a INT NOT NULL, b TEXT) STRICT",
        "INSERT INTO s VALUES (1, 'x')",
    ] {
        let observation = driver
            .send(&Op::Exec(sql.to_string()))
            .expect("the oracle answers");
        assert!(observation.ok, "{sql}: {}", observation.message);
    }
    drop(driver);

    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    assert!(run(&connection, "INSERT INTO s VALUES ('abc', 'y')").is_err());
    run_all(&connection, &["INSERT INTO s VALUES (2, 'y')"]);
    assert_eq!(
        run(&connection, "SELECT a FROM s ORDER BY a"),
        Ok(vec!["int:1".to_string(), "int:2".to_string()])
    );
}

/// `EXPLAIN` and `EXPLAIN QUERY PLAN` answer with rows about a statement
/// rather than running it.
#[test]
fn explain_reports_without_running() {
    let path = scratch("explain");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, team TEXT)",
            "CREATE INDEX t_team ON t (team)",
            "INSERT INTO t VALUES (1, 'ada', 'blue')",
            "INSERT INTO t VALUES (2, 'bob', 'red')",
        ],
    );

    // The plan names the access path, which is the question a person asks it.
    let scan = run(&connection, "EXPLAIN QUERY PLAN SELECT * FROM t").expect("the plan explains");
    assert_eq!(scan.len(), 1);
    assert!(
        scan.first().is_some_and(|line| line.contains("SCAN t")),
        "{scan:?}"
    );
    let search = run(
        &connection,
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE team = 'blue'",
    )
    .expect("the plan explains");
    assert!(
        search
            .first()
            .is_some_and(|line| line.contains("SEARCH t USING INDEX t_team")),
        "{search:?}"
    );
    let sorted = run(
        &connection,
        "EXPLAIN QUERY PLAN SELECT * FROM t ORDER BY name",
    )
    .expect("the plan explains");
    assert!(
        sorted
            .iter()
            .any(|line| line.contains("USE TEMP B-TREE FOR ORDER BY")),
        "{sorted:?}"
    );

    // The bytecode listing has SQLite's eight columns and begins at Init.
    let bytecode = run(&connection, "EXPLAIN SELECT * FROM t").expect("the bytecode explains");
    assert!(!bytecode.is_empty());
    assert!(
        bytecode.first().is_some_and(|line| line.contains("Init")),
        "{bytecode:?}"
    );

    // Explaining does not run: the table is untouched, and a statement that
    // would fail to compile still fails.
    assert_eq!(
        run(&connection, "SELECT count(*) FROM t"),
        Ok(vec!["int:2".to_string()])
    );
    run_all(&connection, &["EXPLAIN DELETE FROM t"]);
    assert_eq!(
        run(&connection, "SELECT count(*) FROM t"),
        Ok(vec!["int:2".to_string()])
    );
    assert!(run(&connection, "EXPLAIN SELECT * FROM nosuchtable").is_err());
}
