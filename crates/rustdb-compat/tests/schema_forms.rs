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
    sqlite_writes_then_reads(path, &[], queries);
}

/// As [`sqlite_reads`], with statements the reference runs first.
///
/// A stored trigger is only really proved by having the *reference* fire it, and
/// that needs the reference to write. The writes go through `exec` one statement
/// at a time because the oracle prepares one statement per request.
fn sqlite_writes_then_reads(path: &PathBuf, writes: &[&str], queries: &[(&str, &[&str])]) {
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
    for sql in writes {
        let observation = driver
            .send(&Op::Exec((*sql).to_string()))
            .expect("the oracle answers");
        assert!(observation.ok, "{sql}: {}", observation.message);
    }
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

/// `REINDEX` rebuilds an index, and the rebuilt one finds the same rows.
///
/// The check that matters is the one SQLite makes: after the rebuild the file
/// still passes `integrity_check`, which walks every index entry against the
/// table it indexes. An index rebuilt wrongly is invisible to a query that uses
/// it - the query simply returns the wrong rows.
#[test]
fn reindex_rebuilds_an_index() {
    let path = scratch("reindex");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, team TEXT COLLATE NOCASE)",
            "CREATE INDEX t_name ON t (name)",
            "CREATE INDEX t_team ON t (team)",
            "CREATE UNIQUE INDEX t_unique ON t (name, team)",
            "INSERT INTO t VALUES (1, 'ada', 'Blue')",
            "INSERT INTO t VALUES (2, 'bob', 'red')",
            "INSERT INTO t VALUES (3, 'cai', 'BLUE')",
            "DELETE FROM t WHERE id = 2",
            "INSERT INTO t VALUES (4, 'dee', 'green')",
        ],
    );
    let before = run(&connection, "SELECT id FROM t WHERE name = 'cai'");

    // Every form: one index, one table's worth, a collation's worth, and all.
    run_all(
        &connection,
        &["REINDEX t_name", "REINDEX t", "REINDEX NOCASE", "REINDEX"],
    );
    assert_eq!(
        run(&connection, "SELECT id FROM t WHERE name = 'cai'"),
        before
    );
    assert_eq!(
        run(
            &connection,
            "SELECT id FROM t WHERE team = 'blue' ORDER BY id"
        ),
        Ok(vec!["int:1".to_string(), "int:3".to_string()])
    );
    assert_eq!(
        run(&connection, "SELECT count(*) FROM t"),
        Ok(vec!["int:3".to_string()])
    );
    // The unique index still refuses a duplicate, so it really was rebuilt with
    // its entries rather than merely emptied.
    assert!(run(&connection, "INSERT INTO t VALUES (5, 'ada', 'blue')").is_err());
    assert!(run(&connection, "REINDEX nosuchthing").is_err());
    drop(connection);
    drop(database);

    sqlite_reads(
        &path,
        &[
            ("SELECT id FROM t WHERE name = 'cai'", &["int:3"]),
            ("SELECT count(*) FROM t", &["int:3"]),
        ],
    );
}

/// Generated columns, `VIRTUAL` and `STORED`.
///
/// The `VIRTUAL` half is the one that can go quietly wrong: the column takes no
/// slot in the record, so every column declared after it sits one place
/// earlier, and a reader that used the declared position would return the
/// neighbouring column's value rather than fail.
#[test]
fn generated_columns_round_trip_through_sqlite() {
    let path = scratch("generated");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE t (
               a INTEGER PRIMARY KEY,
               b INTEGER,
               doubled INTEGER GENERATED ALWAYS AS (b * 2) VIRTUAL,
               c TEXT,
               shouted TEXT GENERATED ALWAYS AS (upper(c)) STORED,
               chained INTEGER AS (doubled + 1) VIRTUAL
             )",
            "CREATE INDEX t_shouted ON t (shouted)",
            "INSERT INTO t (a, b, c) VALUES (1, 21, 'ada')",
            "INSERT INTO t (a, b, c) VALUES (2, 5, 'bob')",
            "INSERT INTO t (a, b, c) VALUES (3, NULL, NULL)",
        ],
    );
    // The column after the VIRTUAL one still reads as itself.
    assert_eq!(
        run(
            &connection,
            "SELECT a, b, doubled, c, shouted, chained FROM t ORDER BY a"
        ),
        Ok(vec![
            "int:1|int:21|int:42|text:ada|text:ADA|int:43".to_string(),
            "int:2|int:5|int:10|text:bob|text:BOB|int:11".to_string(),
            "int:3|null|null|null|null|null".to_string(),
        ])
    );
    assert_eq!(
        run(&connection, "SELECT a FROM t WHERE doubled = 42"),
        Ok(vec!["int:1".to_string()])
    );
    // A STORED generated column is indexable, and the index finds it.
    assert_eq!(
        run(&connection, "SELECT a FROM t WHERE shouted = 'BOB'"),
        Ok(vec!["int:2".to_string()])
    );

    // Writing one is refused, and so are the shapes SQLite refuses.
    assert!(run(&connection, "INSERT INTO t (a, doubled) VALUES (9, 1)").is_err());
    assert!(run(&connection, "CREATE TABLE bad (a, b AS (a) DEFAULT 1)").is_err());
    assert!(run(
        &connection,
        "CREATE TABLE bad (a, b INTEGER PRIMARY KEY AS (a))"
    )
    .is_err());
    assert!(run(&connection, "CREATE TABLE bad (a, b AS (nosuch))").is_err());
    assert!(run(&connection, "CREATE TABLE bad (a, b AS (c), c AS (b))").is_err());
    drop(connection);
    drop(database);

    sqlite_reads(
        &path,
        &[
            (
                "SELECT a, b, doubled, c, shouted, chained FROM t ORDER BY a",
                &[
                    "int:1|int:21|int:42|text:ada|text:ADA|int:43",
                    "int:2|int:5|int:10|text:bob|text:BOB|int:11",
                    "int:3|null|null|null|null|null",
                ],
            ),
            ("SELECT a FROM t WHERE shouted = 'BOB'", &["int:2"]),
        ],
    );
}

/// `ALTER TABLE`, all four forms, with the dependent objects rewritten.
///
/// The rewrite is the part that matters. A rename has to change the table's own
/// `CREATE` text and the text of every index, view and trigger that names it -
/// and it has to leave alone the string literal that happens to contain the
/// same word. A file whose schema was rewritten wrongly still opens; it fails
/// later, when something reads it.
#[test]
fn alter_table_rewrites_the_schema() {
    let path = scratch("alter");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, note TEXT DEFAULT 'about t')",
            "CREATE INDEX t_name ON t (name)",
            "CREATE VIEW t_view AS SELECT id, name FROM t WHERE name IS NOT NULL",
            "INSERT INTO t VALUES (1, 'ada', 'first')",
            "INSERT INTO t VALUES (2, 'bob', 'second')",
        ],
    );

    // ADD COLUMN: existing rows read the default back rather than NULL, which
    // is the half a reader that answered NULL would get wrong.
    run_all(
        &connection,
        &[
            "ALTER TABLE t ADD COLUMN score INTEGER DEFAULT 7",
            "ALTER TABLE t ADD COLUMN spare TEXT",
        ],
    );
    assert_eq!(
        run(&connection, "SELECT id, score, spare FROM t ORDER BY id"),
        Ok(vec![
            "int:1|int:7|null".to_string(),
            "int:2|int:7|null".to_string(),
        ])
    );
    run_all(&connection, &["INSERT INTO t (id, name) VALUES (3, 'cai')"]);
    assert_eq!(
        run(&connection, "SELECT score FROM t WHERE id = 3"),
        Ok(vec!["int:7".to_string()])
    );
    // The shapes SQLite refuses.
    assert!(run(
        &connection,
        "ALTER TABLE t ADD COLUMN k INTEGER PRIMARY KEY"
    )
    .is_err());
    assert!(run(&connection, "ALTER TABLE t ADD COLUMN u TEXT UNIQUE").is_err());
    assert!(run(&connection, "ALTER TABLE t ADD COLUMN n TEXT NOT NULL").is_err());
    assert!(run(&connection, "ALTER TABLE t ADD COLUMN name TEXT").is_err());
    assert!(run(&connection, "ALTER TABLE nosuch ADD COLUMN a").is_err());
    assert!(run(&connection, "ALTER TABLE t_view ADD COLUMN a").is_err());

    // RENAME COLUMN: the table's own text and the index that names it.
    run_all(&connection, &["ALTER TABLE t RENAME COLUMN name TO label"]);
    assert_eq!(
        run(&connection, "SELECT label FROM t WHERE id = 1"),
        Ok(vec!["text:ada".to_string()])
    );
    assert_eq!(
        run(&connection, "SELECT id FROM t WHERE label = 'bob'"),
        Ok(vec!["int:2".to_string()])
    );
    assert!(run(&connection, "SELECT name FROM t").is_err());
    assert!(run(&connection, "ALTER TABLE t RENAME COLUMN nosuch TO x").is_err());
    assert!(run(&connection, "ALTER TABLE t RENAME COLUMN label TO id").is_err());

    // DROP COLUMN: the definition and every row's record.
    run_all(&connection, &["ALTER TABLE t DROP COLUMN spare"]);
    assert!(run(&connection, "SELECT spare FROM t").is_err());
    assert_eq!(
        run(
            &connection,
            "SELECT id, label, note, score FROM t ORDER BY id"
        ),
        Ok(vec![
            "int:1|text:ada|text:first|int:7".to_string(),
            "int:2|text:bob|text:second|int:7".to_string(),
            "int:3|text:cai|text:about t|int:7".to_string(),
        ])
    );
    assert!(run(&connection, "ALTER TABLE t DROP COLUMN id").is_err());
    assert!(run(&connection, "ALTER TABLE t DROP COLUMN label").is_err());

    // RENAME TO: the table, its index, and the view that selects from it.
    run_all(&connection, &["ALTER TABLE t RENAME TO renamed"]);
    assert_eq!(
        run(&connection, "SELECT id FROM renamed WHERE label = 'ada'"),
        Ok(vec!["int:1".to_string()])
    );
    assert_eq!(
        run(&connection, "SELECT count(*) FROM t_view"),
        Ok(vec!["int:3".to_string()])
    );
    assert!(run(&connection, "SELECT * FROM t").is_err());
    // The default's string literal still says `t`: a byte substitution would
    // have rewritten it along with the name.
    assert_eq!(
        run(&connection, "SELECT note FROM renamed WHERE id = 3"),
        Ok(vec!["text:about t".to_string()])
    );
    drop(connection);
    drop(database);

    sqlite_reads(
        &path,
        &[
            (
                "SELECT id, label, note, score FROM renamed ORDER BY id",
                &[
                    "int:1|text:ada|text:first|int:7",
                    "int:2|text:bob|text:second|int:7",
                    "int:3|text:cai|text:about t|int:7",
                ],
            ),
            ("SELECT count(*) FROM t_view", &["int:3"]),
            ("SELECT id FROM renamed WHERE label = 'bob'", &["int:2"]),
        ],
    );
}

/// Triggers rust-db wrote, fired by SQLite itself.
///
/// The differential suite proves the two engines agree while each drives its
/// own file. This proves the *stored* trigger is SQLite's: the pinned binary
/// opens a database rust-db created, writes the table, and its own trigger
/// programs run out of the schema text rust-db wrote. A trigger stored under
/// the wrong `tbl_name`, or with SQL the reference cannot re-parse, passes every
/// test in this engine and fails here.
#[test]
fn triggers_round_trip_through_sqlite() {
    let path = scratch("triggers");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER)",
            "CREATE TABLE audit (seq INTEGER PRIMARY KEY, what TEXT, detail TEXT)",
            "CREATE TRIGGER t_ai AFTER INSERT ON t BEGIN
               INSERT INTO audit (what, detail) VALUES ('insert', new.name);
             END",
            "CREATE TRIGGER t_ad AFTER DELETE ON t BEGIN
               INSERT INTO audit (what, detail) VALUES ('delete', old.name);
             END",
            "CREATE TRIGGER t_au AFTER UPDATE OF name ON t BEGIN
               INSERT INTO audit (what, detail) VALUES ('rename', old.name || '>' || new.name);
             END",
            "CREATE TRIGGER t_guard BEFORE INSERT ON t WHEN new.score < 0 BEGIN
               SELECT RAISE(ABORT, 'score must not be negative');
             END",
            "INSERT INTO t VALUES (1, 'ada', 10)",
            "UPDATE t SET name = 'ada2' WHERE id = 1",
            "DELETE FROM t WHERE id = 1",
        ],
    );
    assert!(run(&connection, "INSERT INTO t VALUES (2, 'bob', -1)").is_err());
    assert_eq!(
        run(&connection, "SELECT what, detail FROM audit ORDER BY seq"),
        Ok(vec![
            "text:insert|text:ada".to_string(),
            "text:rename|text:ada>ada2".to_string(),
            "text:delete|text:ada2".to_string(),
        ])
    );
    drop(connection);
    drop(database);

    sqlite_reads(
        &path,
        &[(
            "SELECT name, tbl_name FROM sqlite_schema WHERE type = 'trigger' ORDER BY name",
            &[
                "text:t_ad|text:t",
                "text:t_ai|text:t",
                "text:t_au|text:t",
                "text:t_guard|text:t",
            ],
        )],
    );
    // The reference writes the table, and its own trigger programs run out of
    // the schema text rust-db wrote.
    sqlite_writes_then_reads(
        &path,
        &["INSERT INTO t VALUES (9, 'zoe', 1)"],
        &[(
            "SELECT what, detail FROM audit ORDER BY seq",
            &[
                "text:insert|text:ada",
                "text:rename|text:ada>ada2",
                "text:delete|text:ada2",
                "text:insert|text:zoe",
            ],
        )],
    );
}

/// `INSTEAD OF` triggers, and the writable view they make.
#[test]
fn instead_of_triggers_round_trip_through_sqlite() {
    let path = scratch("instead-of");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE person (id INTEGER PRIMARY KEY, name TEXT, secret TEXT)",
            "INSERT INTO person VALUES (1, 'ada', 'hidden')",
            "CREATE VIEW public AS SELECT id, name FROM person",
            "CREATE TRIGGER public_ins INSTEAD OF INSERT ON public BEGIN
               INSERT INTO person (id, name, secret) VALUES (new.id, new.name, 'unset');
             END",
            "CREATE TRIGGER public_upd INSTEAD OF UPDATE ON public BEGIN
               UPDATE person SET name = new.name WHERE id = old.id;
             END",
            "CREATE TRIGGER public_del INSTEAD OF DELETE ON public BEGIN
               DELETE FROM person WHERE id = old.id;
             END",
            "INSERT INTO public VALUES (2, 'bob')",
            "UPDATE public SET name = 'robert' WHERE id = 2",
            "INSERT INTO public VALUES (3, 'cai')",
            "DELETE FROM public WHERE id = 3",
        ],
    );
    assert_eq!(
        run(
            &connection,
            "SELECT id, name, secret FROM person ORDER BY id"
        ),
        Ok(vec![
            "int:1|text:ada|text:hidden".to_string(),
            "int:2|text:robert|text:unset".to_string(),
        ])
    );
    drop(connection);
    drop(database);

    sqlite_reads(
        &path,
        &[(
            "SELECT id, name, secret FROM person ORDER BY id",
            &["int:1|text:ada|text:hidden", "int:2|text:robert|text:unset"],
        )],
    );
    // The reference writes the view through rust-db's stored INSTEAD OF trigger.
    sqlite_writes_then_reads(
        &path,
        &["INSERT INTO public VALUES (4, 'dee')"],
        &[(
            "SELECT id, name, secret FROM person ORDER BY id",
            &[
                "int:1|text:ada|text:hidden",
                "int:2|text:robert|text:unset",
                "int:4|text:dee|text:unset",
            ],
        )],
    );
}

/// The trigger shapes SQLite has no grammar for.
///
/// `FOR EACH STATEMENT` is the named one: SQLite has only row triggers, and it
/// is a syntax error rather than a trigger that fires once per statement. An
/// engine that accepted it would store a trigger the reference cannot read.
#[test]
fn the_trigger_forms_sqlite_omits_are_refused() {
    let path = scratch("trigger-omissions");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
            "CREATE VIEW v AS SELECT id, name FROM t",
        ],
    );
    for sql in [
        "CREATE TRIGGER a AFTER INSERT ON t FOR EACH STATEMENT BEGIN SELECT 1; END",
        "CREATE TRIGGER a BEFORE INSERT ON t FOR EACH STATEMENT BEGIN SELECT 1; END",
        "CREATE TRIGGER a AFTER INSERT ON t BEGIN END",
        "CREATE TRIGGER a AFTER INSERT ON t BEGIN SELECT 1 END",
        "CREATE TRIGGER a AFTER INSERT ON t BEGIN CREATE TABLE u (a); END",
        "CREATE TRIGGER a AFTER INSERT ON t BEGIN DROP TABLE t; END",
        "CREATE TRIGGER a AFTER INSERT ON t BEGIN BEGIN; END",
        "CREATE TRIGGER a AFTER INSERT ON t BEGIN INSERT INTO t VALUES (1, 'x') RETURNING id; END",
        "CREATE TRIGGER a AFTER INSERT ON nosuchtable BEGIN SELECT 1; END",
        "CREATE TRIGGER sqlite_a AFTER INSERT ON t BEGIN SELECT 1; END",
        "CREATE TRIGGER a BEFORE INSERT ON v BEGIN SELECT 1; END",
        "CREATE TRIGGER a AFTER INSERT ON v BEGIN SELECT 1; END",
        "CREATE TRIGGER a INSTEAD OF INSERT ON t BEGIN SELECT 1; END",
        "SELECT RAISE(ABORT, 'not in a trigger')",
        "SELECT RAISE(IGNORE)",
        "DROP TRIGGER nosuchtrigger",
        "UPDATE v SET name = 'x'",
        "DELETE FROM v",
        "INSERT INTO v VALUES (1, 'x')",
    ] {
        assert!(
            run(&connection, sql).is_err(),
            "rust-db accepted `{sql}`, which SQLite 3.53.4 refuses"
        );
    }
    // Nothing was created by any of them.
    assert_eq!(
        run(
            &connection,
            "SELECT count(*) FROM sqlite_schema WHERE type = 'trigger'"
        ),
        Ok(vec!["int:0".to_string()])
    );
    drop(connection);
    drop(database);
    sqlite_reads(&path, &[("SELECT count(*) FROM sqlite_schema", &["int:2"])]);
}

/// A `WITHOUT ROWID` table rust-db wrote, read and written by SQLite.
///
/// The b-tree at such a table's root is an *index* b-tree whose record is the
/// row with the primary key moved to the front. Every part of that is a file
/// format claim, so the only test worth having is the reference opening the
/// file: a page created as a table b-tree, or a record left in declaration
/// order, is something rust-db would read back perfectly and SQLite would not.
#[test]
fn without_rowid_round_trips_through_sqlite() {
    let path = scratch("without-rowid");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE w (a TEXT, b INTEGER, c TEXT, PRIMARY KEY (b, a)) WITHOUT ROWID",
            "INSERT INTO w VALUES ('x', 2, 'cx')",
            "INSERT INTO w VALUES ('y', 1, 'cy')",
            "INSERT INTO w VALUES ('z', 3, NULL)",
            "CREATE INDEX w_c ON w (c)",
            "INSERT INTO w VALUES ('q', 9, 'cq')",
            "UPDATE w SET c = 'updated' WHERE b = 1",
            "DELETE FROM w WHERE b = 3",
            "CREATE TABLE k (id TEXT PRIMARY KEY, v REAL) WITHOUT ROWID",
            "INSERT INTO k VALUES ('b', 2.5), ('a', 1.5)",
        ],
    );
    assert_eq!(
        run(&connection, "SELECT a, b, c FROM w ORDER BY b, a"),
        Ok(vec![
            "text:y|int:1|text:updated".to_string(),
            "text:x|int:2|text:cx".to_string(),
            "text:q|int:9|text:cq".to_string(),
        ])
    );
    // A table with no rowid has no `rowid` column, in any of its spellings.
    assert!(run(&connection, "SELECT rowid FROM w").is_err());
    assert!(run(&connection, "SELECT oid FROM k").is_err());
    // The key is unique and implicitly NOT NULL.
    assert!(run(&connection, "INSERT INTO w VALUES ('x', 2, 'dup')").is_err());
    assert!(run(&connection, "INSERT INTO w VALUES (NULL, 5, 'nullkey')").is_err());
    // And a WITHOUT ROWID table must declare one.
    assert!(run(&connection, "CREATE TABLE nokey (a) WITHOUT ROWID").is_err());
    drop(connection);
    drop(database);

    sqlite_reads(
        &path,
        &[
            (
                "SELECT a, b, c FROM w ORDER BY b, a",
                &[
                    "text:y|int:1|text:updated",
                    "text:x|int:2|text:cx",
                    "text:q|int:9|text:cq",
                ],
            ),
            // Read back through the secondary index, whose entries end with the
            // primary key rather than with a rowid.
            ("SELECT a, b FROM w WHERE c = 'cq'", &["text:q|int:9"]),
            (
                "SELECT id, v FROM k ORDER BY id",
                &["text:a|real:1.5", "text:b|real:2.5"],
            ),
            // The primary key gets no `sqlite_autoindex` row of its own: the
            // table's own root is that index.
            (
                "SELECT type, name FROM sqlite_schema ORDER BY name",
                &[
                    "text:table|text:k",
                    "text:table|text:w",
                    "text:index|text:w_c",
                ],
            ),
        ],
    );
    // The reference writes it, and rust-db reads what it wrote.
    sqlite_writes_then_reads(
        &path,
        &[
            "INSERT INTO w VALUES ('r', 4, 'cr')",
            "UPDATE w SET c = 'by-sqlite' WHERE b = 2",
            "DELETE FROM w WHERE b = 9",
        ],
        &[(
            "SELECT a, b, c FROM w ORDER BY b, a",
            &[
                "text:y|int:1|text:updated",
                "text:x|int:2|text:by-sqlite",
                "text:r|int:4|text:cr",
            ],
        )],
    );
    let database = Database::open(&path).expect("the database re-opens");
    let connection = database.connect().expect("the connection re-opens");
    assert_eq!(
        run(&connection, "SELECT a, b, c FROM w ORDER BY b, a"),
        Ok(vec![
            "text:y|int:1|text:updated".to_string(),
            "text:x|int:2|text:by-sqlite".to_string(),
            "text:r|int:4|text:cr".to_string(),
        ])
    );
    assert_eq!(
        run(&connection, "SELECT a, b FROM w WHERE c = 'cr'"),
        Ok(vec!["text:r|int:4".to_string()])
    );
}
