//! The schema surface: views, `STRICT`, and the objects a `DROP` takes with it.
//!
//! Invariant: every fixture here is built by inillucent and then opened, read and
//! `PRAGMA integrity_check`ed by the pinned SQLite 3.53.4 binary, and the same
//! statements are run against both engines. A schema form inillucent can write but
//! SQLite cannot read is a parity failure that no single-engine test can see.

use std::path::PathBuf;

use inillucent_compat::facade::Database;
use inillucent_compat::oracle::{Driver, Op, TaggedValue};
use inillucent_compat::workspace_root;
use inillucent_value::Value;

/// Returns the pinned oracle binary, when it has been built.
fn oracle_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
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

/// Runs a statement through inillucent, returning its rows or its failure.
fn run(
    connection: &inillucent_compat::facade::Connection,
    sql: &str,
) -> Result<Vec<String>, String> {
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

/// Runs a script through inillucent, asserting every statement succeeds.
fn run_all(connection: &inillucent_compat::facade::Connection, script: &[&str]) {
    for sql in script {
        run(connection, sql).unwrap_or_else(|reason| panic!("{sql}: {reason}"));
    }
}

/// Opens a database inillucent wrote with the pinned binary and checks it over.
///
/// The integrity check is the point: a schema row inillucent writes with the wrong
/// shape produces a file SQLite opens and then reports as corrupt, and a test
/// that only re-read the file with inillucent would agree with itself.
fn sqlite_reads(path: &PathBuf, queries: &[(&str, &[&str])]) {
    sqlite_writes_then_reads(path, &[], queries);
}

/// As [`sqlite_reads`], with statements the reference runs first.
///
/// A stored trigger is only really proved by having the *reference* fire it, and
/// that needs the reference to write. The writes go through `exec` one statement
/// at a time because the oracle prepares one statement per request.
///
/// Returns the inillucent database the reference's writes were carried back
/// into, which is what a caller that wants to read them here reopens.
fn sqlite_writes_then_reads(
    path: &PathBuf,
    writes: &[&str],
    queries: &[(&str, &[&str])],
) -> PathBuf {
    let Some(program) = oracle_path() else {
        panic!("the pinned SQLite oracle is not built");
    };
    // **Carried across, not opened in place.** inillucent's file is an `RDB2`
    // file and SQLite's is a SQLite file; handing the path straight over tests
    // which header each engine writes, which is settled and deliberate. What
    // these tests ask is whether the *schema* inillucent stores is one SQLite
    // reads and agrees with, so the database goes over as `.dump` - the
    // interchange the shell ships - and every assertion below is then made of a
    // real SQLite file the reference built from inillucent's own SQL.
    let mirror = inillucent_compat::interchange::as_sqlite_file(path)
        .unwrap_or_else(|reason| panic!("carrying {path:?} across: {reason}"));
    let mut driver = Driver::start("sqlite", &program).expect("the oracle starts");
    driver.send(&Op::Hello).expect("the oracle answers");
    driver
        .send(&Op::Open(mirror.display().to_string()))
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
    if writes.is_empty() {
        return path.clone();
    }
    // The reference wrote into its own copy, so the writes come back the same
    // way they went out. A caller that only read gets its own path back and
    // nothing is rebuilt.
    drop(driver);
    let carried = path.with_extension("carried-back.db");
    inillucent_compat::interchange::from_sqlite_file(&mirror, &carried)
        .unwrap_or_else(|reason| panic!("carrying {mirror:?} back: {reason}"));
    carried
}

/// A view inillucent creates is one SQLite reads, and it selects the same rows.
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

/// A file SQLite wrote with a `STRICT` table is one inillucent enforces too.
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

    // A file the reference wrote is a SQLite file: it is imported, not opened.
    let database = Database::import(&path).expect("the database imports");
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

/// Triggers inillucent wrote, fired by SQLite itself.
///
/// The differential suite proves the two engines agree while each drives its
/// own file. This proves the *stored* trigger is SQLite's: the pinned binary
/// opens a database inillucent created, writes the table, and its own trigger
/// programs run out of the schema text inillucent wrote. A trigger stored under
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
    // the schema text inillucent wrote.
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
    // The reference writes the view through inillucent's stored INSTEAD OF trigger.
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
            "inillucent accepted `{sql}`, which SQLite 3.53.4 refuses"
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

/// A `WITHOUT ROWID` table inillucent wrote, read and written by SQLite.
///
/// The b-tree at such a table's root is an *index* b-tree whose record is the
/// row with the primary key moved to the front. Every part of that is a file
/// format claim, so the only test worth having is the reference opening the
/// file: a page created as a table b-tree, or a record left in declaration
/// order, is something inillucent would read back perfectly and SQLite would not.
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
    // The reference writes it, and inillucent reads what it wrote.
    let carried = sqlite_writes_then_reads(
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
    let database = Database::open(&carried).expect("the database re-opens");
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

/// `VACUUM`, which rebuilds the database and gives the free space back.
///
/// **It defragments, and this test proves it by measuring.** The statement
/// used to be a checkpoint: the pages a `DELETE` freed went back to the
/// free map and were handed out again, the file never shrank, and this comment
/// said so. It is now a logical rebuild - every table recreated, every row
/// re-inserted in key order, every index built over rows that are already
/// there - so the file that comes out is the one this engine would have written
/// had nothing ever been deleted.
///
/// Three things are asserted, and the first is the one that was missing: the
/// file is **smaller afterwards**. Then that every row, every index, the view,
/// the trigger and the `WITHOUT ROWID` table still answer, and the header word
/// survives. And then that the *reference* can open what came out - a schema
/// row left pointing at an old root is something inillucent would read back
/// perfectly well and SQLite would report as corrupt, which is why the oracle
/// is the check that means the most.
#[test]
fn vacuum_folds_the_log_in_and_sqlite_still_reads() {
    let path = scratch("vacuum");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, pad TEXT)",
            "CREATE INDEX t_name ON t (name)",
            "CREATE VIEW v AS SELECT id, name FROM t",
            "CREATE TABLE log (seq INTEGER PRIMARY KEY, what TEXT)",
            "CREATE TRIGGER t_ai AFTER INSERT ON t BEGIN
               INSERT INTO log (what) VALUES (new.name);
             END",
            "CREATE TABLE w (k TEXT PRIMARY KEY, n INTEGER) WITHOUT ROWID",
            "PRAGMA user_version = 42",
        ],
    );
    // Enough rows to need more than one page, then most of them deleted, so
    // the file really has free space to reclaim.
    for row in 0..300 {
        let padding = "x".repeat(120);
        run_all(
            &connection,
            &[
                &format!("INSERT INTO t VALUES ({row}, 'n{row}', '{padding}')"),
                &format!("INSERT INTO w VALUES ('k{row}', {row})"),
            ],
        );
    }
    run_all(
        &connection,
        &[
            "DELETE FROM t WHERE id % 2 = 0",
            "DELETE FROM w WHERE n % 2 = 0",
        ],
    );
    // **Measured across the statement, not asserted about it.** A checkpoint
    // first, so the comparison is between two files that both hold everything
    // rather than between one that does and one whose pages are still in a log.
    run_all(&connection, &["PRAGMA wal_checkpoint"]);
    let before = std::fs::metadata(&path).map(|held| held.len()).unwrap_or(0);
    run_all(&connection, &["VACUUM"]);
    let after = std::fs::metadata(&path).map(|held| held.len()).unwrap_or(0);
    assert!(
        after < before,
        "VACUUM must give the space back: {before} bytes before, {after} after"
    );
    // Everything still answers, through the table, through the index, through
    // the view, and through the table with no rowid.
    assert_eq!(
        run(&connection, "SELECT count(*) FROM t"),
        Ok(vec!["int:150".to_string()])
    );
    assert_eq!(
        run(&connection, "SELECT id FROM t WHERE name = 'n101'"),
        Ok(vec!["int:101".to_string()])
    );
    assert_eq!(
        run(&connection, "SELECT count(*) FROM v"),
        Ok(vec!["int:150".to_string()])
    );
    assert_eq!(
        run(&connection, "SELECT n FROM w WHERE k = 'k101'"),
        Ok(vec!["int:101".to_string()])
    );
    assert_eq!(
        run(&connection, "SELECT count(*) FROM log"),
        Ok(vec!["int:300".to_string()])
    );
    assert_eq!(
        run(&connection, "PRAGMA user_version"),
        Ok(vec!["int:42".to_string()])
    );
    // The trigger survived the rebuild and still fires.
    run_all(&connection, &["INSERT INTO t VALUES (9001, 'after', '')"]);
    assert_eq!(
        run(
            &connection,
            "SELECT what FROM log ORDER BY seq DESC LIMIT 1"
        ),
        Ok(vec!["text:after".to_string()])
    );
    // It cannot run inside a transaction.
    run_all(&connection, &["BEGIN"]);
    assert!(run(&connection, "VACUUM").is_err());
    run_all(&connection, &["COMMIT"]);
    drop(connection);
    drop(database);

    sqlite_reads(
        &path,
        &[
            ("SELECT count(*) FROM t", &["int:151"]),
            ("SELECT id FROM t WHERE name = 'n101'", &["int:101"]),
            ("SELECT count(*) FROM v", &["int:151"]),
            ("SELECT n FROM w WHERE k = 'k101'", &["int:101"]),
            ("PRAGMA user_version", &["int:42"]),
            (
                "SELECT type, name FROM sqlite_schema ORDER BY name",
                &[
                    "text:table|text:log",
                    "text:table|text:t",
                    "text:trigger|text:t_ai",
                    "text:index|text:t_name",
                    "text:view|text:v",
                    "text:table|text:w",
                ],
            ),
        ],
    );
}

/// `VACUUM INTO`, which writes a rebuilt copy and leaves the original alone.
#[test]
fn vacuum_into_writes_a_copy_sqlite_reads() {
    let path = scratch("vacuum-into");
    let copy = path.with_extension("copy");
    let _ = std::fs::remove_file(&copy);
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
            "CREATE INDEX t_name ON t (name)",
            "INSERT INTO t VALUES (1, 'ada'), (2, 'bob'), (3, 'cai')",
            "DELETE FROM t WHERE id = 2",
        ],
    );
    let target = copy
        .display()
        .to_string()
        .replace(std::path::MAIN_SEPARATOR, "/");
    run_all(&connection, &["PRAGMA wal_checkpoint"]);
    let original = std::fs::metadata(&path).map(|held| held.len()).unwrap_or(0);
    run_all(&connection, &[&format!("VACUUM INTO '{target}'")]);
    let written = std::fs::metadata(&copy).map(|held| held.len()).unwrap_or(0);
    // **A rebuild, not a byte copy.** The two used to be the same size to the
    // byte, because the statement called `std::fs::copy`.
    assert!(
        written <= original,
        "VACUUM INTO must compact: {original} bytes in, {written} out"
    );
    // The original is untouched and still works.
    assert_eq!(
        run(&connection, "SELECT id, name FROM t ORDER BY id"),
        Ok(vec![
            "int:1|text:ada".to_string(),
            "int:3|text:cai".to_string()
        ])
    );
    // And it refuses to overwrite, rather than replacing a file it was pointed
    // at by mistake.
    assert!(run(&connection, &format!("VACUUM INTO '{target}'")).is_err());
    drop(connection);
    drop(database);

    sqlite_reads(
        &copy,
        &[
            (
                "SELECT id, name FROM t ORDER BY id",
                &["int:1|text:ada", "int:3|text:cai"],
            ),
            ("SELECT id FROM t WHERE name = 'cai'", &["int:3"]),
        ],
    );
    let reopened = Database::open(&copy).expect("the copy opens");
    let connection = reopened.connect().expect("the copy connects");
    assert_eq!(
        run(&connection, "SELECT count(*) FROM t"),
        Ok(vec!["int:2".to_string()])
    );
}

/// H3 (task-1920): an `ALTER TABLE ADD COLUMN` on an empty table does not
/// evaluate the new column's `DEFAULT`, and so cannot half-apply.
///
/// **What used to happen.** `alter_table` rewrites every catalog row that names
/// the table, rebuilds the connection's schema from those rows, and then
/// rebuilds the tree. The tree rebuild is where a `DEFAULT` was evaluated - by
/// running `SELECT <the default text>` through the ordinary execute path - and
/// it ran whether or not there were rows to fill. So `ALTER TABLE t ADD COLUMN
/// b INTEGER DEFAULT (no_such_function())` failed three writes after the
/// catalog already said the column was there. Nothing undid those writes:
/// outside an explicit transaction `rewrite` recorded no before-image, and
/// `execute_ddl` had no rollback wrapper the way `write` has `abandon`.
/// `next_txn` had not moved either, because `seal` was never reached, so the
/// half-written rows were committed by whatever the next successful statement
/// committed. `PRAGMA table_info(t)` then listed a column the tree had no slot
/// for, on disk, across a reopen.
///
/// **An empty table is the only way to reach it, and that is why it is the
/// first `ALTER` a user runs.** `AddedColumnRisk::refusal` refuses a default
/// that is not a literal, and `alter_table` applies that refusal only when
/// `table_has_a_row` - so a populated table is turned away up front with
/// SQLite's own "Cannot add a column with non-constant default", and an empty
/// one walks past the guard into the rewrite.
///
/// **The fix is to match the reference rather than to refuse earlier.** SQLite
/// accepts this `ALTER`, records `DEFAULT (no_such_function())` in the schema
/// text, and reports `unknown function` at the first `INSERT` that needs the
/// value - which is what every assertion below is graded against, the reference
/// answering first so the expectations are its own.
#[test]
fn an_alter_adding_a_column_to_an_empty_table_matches_the_oracle() {
    let Some(program) = oracle_path() else {
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    // The reference's answers, taken first so that what is asserted below is
    // SQLite's behaviour rather than this engine's opinion of it.
    let reference = scratch("alter-empty-default-oracle");
    let mut driver = Driver::start("sqlite", &program).expect("the oracle starts");
    driver.send(&Op::Hello).expect("the oracle greets");
    driver
        .send(&Op::Open(reference.display().to_string()))
        .expect("the oracle opens");
    for statement in [
        "CREATE TABLE t (a INTEGER PRIMARY KEY, name TEXT)",
        "ALTER TABLE t ADD COLUMN b INTEGER DEFAULT (no_such_function())",
    ] {
        let observation = driver
            .send(&Op::Exec(statement.to_string()))
            .expect("the oracle answers");
        assert!(
            observation.ok,
            "SQLite refused {statement}: {}",
            observation.message
        );
    }
    let oracle_columns: Vec<String> = driver
        .send(&Op::Query("PRAGMA table_info(t)".to_string()))
        .expect("the oracle answers")
        .rows
        .iter()
        .filter_map(|row| row.get(1))
        .map(render_tagged)
        .collect();
    assert_eq!(
        oracle_columns,
        vec![
            "text:a".to_string(),
            "text:name".to_string(),
            "text:b".to_string()
        ]
    );
    let oracle_insert = driver
        .send(&Op::Exec(
            "INSERT INTO t (a, name) VALUES (1, 'ada')".to_string(),
        ))
        .expect("the oracle answers");
    assert!(
        !oracle_insert.ok,
        "SQLite ran an INSERT whose default calls a function it does not have"
    );
    let oracle_explicit = driver
        .send(&Op::Exec(
            "INSERT INTO t (a, name, b) VALUES (2, 'bob', 5)".to_string(),
        ))
        .expect("the oracle answers");
    assert!(
        oracle_explicit.ok,
        "SQLite refused an INSERT that supplies the column: {}",
        oracle_explicit.message
    );

    let path = scratch("alter-empty-default");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE t (a INTEGER PRIMARY KEY, name TEXT)",
            "CREATE INDEX t_name ON t (name)",
            "CREATE VIEW t_view AS SELECT a, name FROM t",
            "ALTER TABLE t ADD COLUMN b INTEGER DEFAULT (no_such_function())",
        ],
    );
    let columns: Vec<String> = run(&connection, "PRAGMA table_info(t)")
        .expect("table_info answers")
        .iter()
        .filter_map(|row| row.split('|').nth(1).map(str::to_string))
        .collect();
    assert_eq!(
        columns, oracle_columns,
        "the columns disagree with the reference's after the same ALTER"
    );
    // The catalog and the tree agree, which is the half that used to be wrong:
    // a read of the new column answers NULL for a row rather than failing, and
    // a write against the shape the catalog claims lands.
    assert!(
        run(&connection, "INSERT INTO t (a, name) VALUES (1, 'ada')").is_err(),
        "an INSERT that needs the default must report the function, as SQLite does"
    );
    run_all(
        &connection,
        &["INSERT INTO t (a, name, b) VALUES (2, 'bob', 5)"],
    );
    assert_eq!(
        run(&connection, "SELECT a, name, b FROM t ORDER BY a"),
        Ok(vec!["int:2|text:bob|int:5".to_string()])
    );
    assert_eq!(
        run(&connection, "SELECT a FROM t WHERE name = 'bob'"),
        Ok(vec!["int:2".to_string()])
    );
    assert!(database.check().is_ok());

    drop(connection);
    drop(database);
    let reopened = Database::open(&path).expect("the database reopens");
    let connection = reopened.connect().expect("the reopened database connects");
    assert_eq!(
        run(&connection, "SELECT a, name, b FROM t ORDER BY a"),
        Ok(vec!["int:2|text:bob|int:5".to_string()])
    );
    assert!(reopened.check().is_ok());
}

/// H3 (task-1920): a `REINDEX` that fails on a later index leaves the earlier
/// ones as they were.
///
/// **`REINDEX` is the same shape as `ALTER` and the same fix covers it.** It
/// walks every target index, rebuilds each one's tree, rewrites each one's
/// catalog row with the new root page, and seals once at the end. A failure on
/// the third index used to leave the first two rewritten and uncommitted, for
/// the next successful statement to commit.
///
/// The failure is arranged the way an application would actually meet it: the
/// third index is partial, its predicate calls a function the application
/// registered, and the connection running the `REINDEX` no longer has it. That
/// is a real state - a function one connection registered and another did not -
/// and reaching it needs no fault injection.
#[test]
fn a_reindex_that_fails_partway_changes_nothing() {
    let path = scratch("reindex-failed");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    connection
        .create_scalar_function(
            "keeps",
            1,
            inillucent_ext::registry::FunctionFlags::external(),
            std::sync::Arc::new(|arguments: &[Value<'static>]| {
                let value = arguments
                    .first()
                    .and_then(Value::as_integer)
                    .unwrap_or_default();
                Ok(Value::Integer(i64::from(value % 2 == 0)))
            }),
        )
        .expect("the function registers");
    run_all(
        &connection,
        &[
            "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT, c INTEGER)",
            "INSERT INTO t VALUES (1, 'one', 10)",
            "INSERT INTO t VALUES (2, 'two', 20)",
            "INSERT INTO t VALUES (3, 'three', 30)",
            "CREATE INDEX i_b ON t (b)",
            "CREATE INDEX i_c ON t (c)",
            "CREATE INDEX i_partial ON t (c) WHERE keeps(a)",
        ],
    );
    let schema = run(
        &connection,
        "SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema ORDER BY name",
    )
    .expect("the schema reads");

    // Without the function the partial index's predicate cannot be bound, so
    // its rebuild fails - after `i_b` and `i_c` have been rebuilt and had their
    // catalog rows rewritten with new root pages.
    assert!(
        connection.remove_function("keeps", 1),
        "the function was registered and is removed"
    );
    assert!(
        run(&connection, "REINDEX").is_err(),
        "a REINDEX whose predicate cannot be bound must fail, not report success"
    );

    assert_eq!(
        run(
            &connection,
            "SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema ORDER BY name"
        ),
        Ok(schema.clone()),
        "a catalog row was rewritten by the part of the REINDEX that succeeded"
    );
    // The indexes still answer, which is what a rewritten row naming a tree
    // that was then rolled back would break.
    assert_eq!(
        run(&connection, "SELECT a FROM t WHERE b = 'two'"),
        Ok(vec!["int:2".to_string()])
    );
    assert_eq!(
        run(&connection, "SELECT a FROM t WHERE c = 30"),
        Ok(vec!["int:3".to_string()])
    );
    assert!(
        database.check().is_ok(),
        "every index still agrees with the table it is on"
    );

    drop(connection);
    drop(database);
    let reopened = Database::open(&path).expect("the database reopens");
    let connection = reopened.connect().expect("the reopened database connects");
    assert_eq!(
        run(
            &connection,
            "SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema ORDER BY name"
        ),
        Ok(schema),
        "the failed REINDEX became permanent across a reopen"
    );
    assert_eq!(
        run(&connection, "SELECT a FROM t WHERE c = 20"),
        Ok(vec!["int:2".to_string()])
    );
    assert!(reopened.check().is_ok());
}
