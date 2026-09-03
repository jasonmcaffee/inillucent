//! rust-db and the pinned SQLite 3.53.4, asked to write the same rows.
//!
//! Invariant: every claim here is a comparison against a live SQLite process,
//! never against a value written into the test. The scripts run against both
//! engines statement by statement, and after each one the reply is compared in
//! full: the rows, their storage classes, `changes`, `total_changes`,
//! `last_insert_rowid`, the autocommit flag, and - when a statement fails - the
//! primary and extended result codes.
//!
//! Comparing the *codes* is what makes this a parity test rather than a smoke
//! test. Any engine can refuse a duplicate key; refusing it as
//! `SQLITE_CONSTRAINT_UNIQUE` rather than `SQLITE_CONSTRAINT_PRIMARYKEY` is
//! what an application's error handling is written against.
//!
//! When the pinned oracle has not been built these tests report what is missing
//! and return, because the compatibility report is driven by recorded results
//! and a run without the oracle should record nothing rather than assume a pass.

use std::path::PathBuf;

use rustdb_compat::oracle::{Driver, Observation, Op, TaggedValue};
use rustdb_compat::workspace_root;
use rustdb_session::connection::{Connection, OpenOptions, SessionDatabase};
use rustdb_session::statement::Statement;
use rustdb_value::Value;

/// Returns the pinned SQLite oracle binary, if it has been built.
fn sqlite_oracle() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("RUSTDB_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let directory = workspace_root().join(".sqlite-ref/3.53.4");
    let path = directory.join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Starts the oracle on a fresh database, or reports why it could not.
fn start_oracle(name: &str) -> Option<Driver> {
    let program = sqlite_oracle()?;
    let path = scratch(name, "sqlite");
    let mut driver = Driver::start("sqlite", &program).ok()?;
    let hello = driver.send(&Op::Hello).ok()?;
    assert!(hello.ok, "the oracle did not answer hello");
    let opened = driver
        .send(&Op::Open(path.display().to_string()))
        .expect("the oracle opens its database");
    assert!(opened.ok, "the oracle could not open {}", path.display());
    Some(driver)
}

/// Returns a fresh path for one engine's copy of one scenario.
fn scratch(name: &str, engine: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output/task-1786/differential");
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join(format!("{name}-{engine}.db"));
    for suffix in ["", "-journal"] {
        let _ = std::fs::remove_file(directory.join(format!("{name}-{engine}.db{suffix}")));
    }
    path
}

/// Opens rust-db on its own copy of a scenario's database.
fn start_rustdb(name: &str) -> Connection {
    let path = scratch(name, "rustdb");
    let database = SessionDatabase::open_with_options(
        &path,
        OpenOptions {
            busy_timeout: std::time::Duration::from_secs(5),
            ..OpenOptions::default()
        },
    )
    .expect("rust-db opens its database");
    database.connect().expect("rust-db connects")
}

/// Runs one statement on rust-db and reports it the way the oracle would.
///
/// The shapes have to match exactly, including the parts that are easy to get
/// almost right: a failed statement still reports the connection state after
/// it, and a query that produced no rows still reports its column names.
fn observe(connection: &Connection, sql: &str, query: bool) -> Observation {
    let mut observation = Observation::default();
    let outcome = (|| -> Result<(Vec<Vec<TaggedValue>>, Vec<String>), rustdb_base::DbError> {
        let mut rows = Vec::new();
        let mut columns = Vec::new();
        let mut offset = 0usize;
        let bytes = sql.as_bytes();
        while offset < bytes.len() {
            let rest = bytes.get(offset..).unwrap_or(&[]);
            let (mut statement, consumed) = Statement::prepare(connection, rest)?;
            if query {
                columns = statement
                    .columns()
                    .iter()
                    .map(|column| String::from_utf8_lossy(&column.name).into_owned())
                    .collect();
            }
            while statement.step()? {
                if query {
                    rows.push(statement.row().iter().map(tagged).collect());
                }
            }
            statement.finalize()?;
            if consumed == 0 {
                break;
            }
            offset = offset.saturating_add(consumed);
        }
        Ok((rows, columns))
    })();
    let counters = connection.counters();
    match outcome {
        Ok((rows, columns)) => {
            observation.ok = true;
            observation.rows = rows;
            observation.columns = columns;
        }
        Err(failure) => {
            observation.ok = false;
            observation.code = failure.code().value();
            observation.extended = failure.extended().value();
            observation.message = failure.message().to_string();
        }
    }
    observation.changes = counters.changes;
    observation.total_changes = counters.total_changes;
    observation.last_insert_rowid = counters.last_insert_rowid;
    observation.autocommit = connection.autocommit();
    observation
}

/// Renders a rust-db value as the tagged value the protocol carries.
fn tagged(value: &Value<'static>) -> TaggedValue {
    match value {
        Value::Null => TaggedValue::Null,
        Value::Integer(integer) => TaggedValue::Integer(*integer),
        Value::Real(real) => TaggedValue::Real(*real),
        Value::Text(text) => TaggedValue::Text(text.utf8_bytes().into_owned()),
        Value::Blob(blob) => TaggedValue::Blob(blob.raw().to_vec()),
    }
}

/// One step of a scenario.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// SQL that is not expected to return rows.
    Exec(&'static str),
    /// SQL whose rows are compared.
    Query(&'static str),
}

/// Runs a scenario against both engines and compares every reply.
///
/// Returns how many statements were compared, so a scenario that silently
/// stopped early cannot look like one that passed.
fn compare(name: &str, steps: &[Step]) -> usize {
    let Some(mut oracle) = start_oracle(name) else {
        eprintln!("the pinned SQLite oracle is not built; run tools/sqlite-reference.{{ps1,sh}}");
        return 0;
    };
    let connection = start_rustdb(name);
    let mut compared = 0usize;
    for (index, step) in steps.iter().enumerate() {
        let (sql, query) = match step {
            Step::Exec(sql) => (*sql, false),
            Step::Query(sql) => (*sql, true),
        };
        let op = if query {
            Op::Query(sql.to_string())
        } else {
            Op::Exec(sql.to_string())
        };
        let reference = oracle.send(&op).expect("the oracle answers");
        let candidate = observe(&connection, sql, query);
        assert_eq!(
            candidate.ok,
            reference.ok,
            "step {index} `{sql}`: rust-db {} and SQLite {}\n  rust-db: {}\n  SQLite:  {}",
            if candidate.ok { "succeeded" } else { "failed" },
            if reference.ok { "succeeded" } else { "failed" },
            candidate.message,
            reference.message
        );
        if !reference.ok {
            assert_eq!(
                candidate.code, reference.code,
                "step {index} `{sql}`: primary code\n  rust-db: {} ({})\n  SQLite:  {} ({})",
                candidate.code, candidate.message, reference.code, reference.message
            );
            assert_eq!(
                candidate.extended, reference.extended,
                "step {index} `{sql}`: extended code\n  rust-db: {} ({})\n  SQLite:  {} ({})",
                candidate.extended, candidate.message, reference.extended, reference.message
            );
        }
        if query {
            assert_eq!(
                candidate.rows.len(),
                reference.rows.len(),
                "step {index} `{sql}`: row count"
            );
            for (row, (left, right)) in candidate.rows.iter().zip(reference.rows.iter()).enumerate()
            {
                assert_eq!(
                    left.len(),
                    right.len(),
                    "step {index} `{sql}` row {row}: width"
                );
                for (column, (candidate, reference)) in left.iter().zip(right.iter()).enumerate() {
                    assert!(
                        candidate.identical(reference),
                        "step {index} `{sql}` row {row} column {column}\n  rust-db: {candidate:?}\n  SQLite:  {reference:?}"
                    );
                }
            }
        }
        assert_eq!(
            candidate.changes, reference.changes,
            "step {index} `{sql}`: changes()"
        );
        assert_eq!(
            candidate.total_changes, reference.total_changes,
            "step {index} `{sql}`: total_changes()"
        );
        assert_eq!(
            candidate.last_insert_rowid, reference.last_insert_rowid,
            "step {index} `{sql}`: last_insert_rowid()"
        );
        assert_eq!(
            candidate.autocommit, reference.autocommit,
            "step {index} `{sql}`: autocommit"
        );
        compared = compared.saturating_add(1);
    }
    let _ = oracle.send(&Op::Bye);
    compared
}

/// The plain CRUD path: create, insert, update, delete, read back.
#[test]
fn crud_matches_sqlite() {
    let compared = compare(
        "crud",
        &[
            Step::Exec("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c REAL)"),
            Step::Exec("INSERT INTO t VALUES(1, 'one', 1.5)"),
            Step::Exec("INSERT INTO t VALUES(2, 'two', 2.5)"),
            Step::Exec("INSERT INTO t(b, c) VALUES('three', 3.5)"),
            Step::Exec("INSERT INTO t DEFAULT VALUES"),
            Step::Query("SELECT a, b, c FROM t ORDER BY a"),
            Step::Exec("UPDATE t SET c = c * 2 WHERE a <= 2"),
            Step::Query("SELECT a, c FROM t ORDER BY a"),
            Step::Exec("DELETE FROM t WHERE a = 2"),
            Step::Query("SELECT a FROM t ORDER BY a"),
            Step::Exec("UPDATE t SET b = 'renamed'"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
            Step::Exec("DELETE FROM t"),
            Step::Query("SELECT count(*) FROM t"),
        ],
    );
    assert!(compared == 0 || compared == 14, "compared {compared} steps");
}

/// Affinity is applied on the way in, exactly as SQLite applies it.
#[test]
fn stored_affinity_matches_sqlite() {
    let compared = compare(
        "affinity",
        &[
            Step::Exec("CREATE TABLE t(i INTEGER, r REAL, t TEXT, b BLOB, n NUMERIC)"),
            Step::Exec("INSERT INTO t VALUES('42', '42', 42, 42, '42')"),
            Step::Exec("INSERT INTO t VALUES('42x', '3.5', 3.5, '3.5', '3.5')"),
            Step::Exec("INSERT INTO t VALUES(NULL, NULL, NULL, NULL, NULL)"),
            Step::Exec("INSERT INTO t VALUES(1.0, 1, x'01', x'01', 1.0)"),
            Step::Query("SELECT i, r, t, b, n FROM t"),
            Step::Query("SELECT typeof(i), typeof(r), typeof(t), typeof(b), typeof(n) FROM t"),
        ],
    );
    assert!(compared == 0 || compared == 7, "compared {compared} steps");
}

/// The rowid rules: allocation, an explicit value, and the largest one.
#[test]
fn rowid_allocation_matches_sqlite() {
    let compared = compare(
        "rowid",
        &[
            Step::Exec("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)"),
            Step::Exec("INSERT INTO t(b) VALUES('first')"),
            Step::Exec("INSERT INTO t VALUES(100, 'hundred')"),
            Step::Exec("INSERT INTO t(b) VALUES('after')"),
            Step::Exec("INSERT INTO t VALUES(NULL, 'null key')"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
            Step::Query("SELECT rowid, a FROM t ORDER BY rowid"),
            Step::Exec("CREATE TABLE u(a TEXT)"),
            Step::Exec("INSERT INTO u VALUES('x')"),
            Step::Exec("INSERT INTO u VALUES('y')"),
            Step::Query("SELECT rowid, a FROM u ORDER BY rowid"),
            Step::Exec("DELETE FROM u WHERE rowid = 2"),
            Step::Exec("INSERT INTO u VALUES('z')"),
            Step::Query("SELECT rowid, a FROM u ORDER BY rowid"),
        ],
    );
    assert!(compared == 0 || compared == 14, "compared {compared} steps");
}

/// Every constraint reports the code SQLite reports.
#[test]
fn constraint_codes_match_sqlite() {
    let compared = compare(
        "constraints",
        &[
            Step::Exec(
                "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT NOT NULL, c INTEGER UNIQUE, d INTEGER CHECK (d > 0))",
            ),
            Step::Exec("INSERT INTO t VALUES(1, 'one', 10, 1)"),
            Step::Exec("INSERT INTO t VALUES(1, 'again', 11, 1)"),
            Step::Exec("INSERT INTO t VALUES(2, NULL, 12, 1)"),
            Step::Exec("INSERT INTO t VALUES(3, 'three', 10, 1)"),
            Step::Exec("INSERT INTO t VALUES(4, 'four', 14, 0)"),
            Step::Exec("INSERT INTO t VALUES(5, 'five', NULL, 1)"),
            Step::Exec("INSERT INTO t VALUES(6, 'six', NULL, 1)"),
            Step::Query("SELECT a, b, c, d FROM t ORDER BY a"),
            Step::Exec("UPDATE t SET c = 10 WHERE a = 5"),
            Step::Exec("UPDATE t SET b = NULL WHERE a = 5"),
            Step::Exec("UPDATE t SET d = -1 WHERE a = 5"),
            Step::Query("SELECT a, b, c, d FROM t ORDER BY a"),
        ],
    );
    assert!(compared == 0 || compared == 13, "compared {compared} steps");
}

/// The five conflict algorithms behave identically.
#[test]
fn conflict_algorithms_match_sqlite() {
    let compared = compare(
        "conflicts",
        &[
            Step::Exec("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT UNIQUE)"),
            Step::Exec("INSERT INTO t VALUES(1, 'one')"),
            Step::Exec("INSERT INTO t VALUES(2, 'two')"),
            Step::Exec("INSERT OR IGNORE INTO t VALUES(1, 'ignored')"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
            Step::Exec("INSERT OR REPLACE INTO t VALUES(1, 'replaced')"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
            Step::Exec("INSERT OR REPLACE INTO t VALUES(3, 'two')"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
            Step::Exec("INSERT OR ABORT INTO t VALUES(1, 'nope')"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
            Step::Exec("INSERT OR FAIL INTO t VALUES(4, 'four'), (1, 'nope'), (5, 'five')"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
            Step::Exec("INSERT OR IGNORE INTO t VALUES(6, 'six'), (1, 'nope'), (7, 'seven')"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
        ],
    );
    assert!(compared == 0 || compared == 15, "compared {compared} steps");
}

/// Transactions and savepoints report the same state and keep the same rows.
#[test]
fn transactions_match_sqlite() {
    let compared = compare(
        "transactions",
        &[
            Step::Exec("CREATE TABLE t(a INTEGER PRIMARY KEY)"),
            Step::Exec("INSERT INTO t VALUES(1)"),
            Step::Exec("BEGIN"),
            Step::Exec("INSERT INTO t VALUES(2)"),
            Step::Query("SELECT a FROM t ORDER BY a"),
            Step::Exec("ROLLBACK"),
            Step::Query("SELECT a FROM t ORDER BY a"),
            Step::Exec("BEGIN"),
            Step::Exec("INSERT INTO t VALUES(3)"),
            Step::Exec("SAVEPOINT s"),
            Step::Exec("INSERT INTO t VALUES(4)"),
            Step::Exec("ROLLBACK TO s"),
            Step::Query("SELECT a FROM t ORDER BY a"),
            Step::Exec("RELEASE s"),
            Step::Exec("COMMIT"),
            Step::Query("SELECT a FROM t ORDER BY a"),
            Step::Exec("SAVEPOINT outer_level"),
            Step::Exec("INSERT INTO t VALUES(5)"),
            Step::Exec("RELEASE outer_level"),
            Step::Query("SELECT a FROM t ORDER BY a"),
        ],
    );
    assert!(compared == 0 || compared == 20, "compared {compared} steps");
}

/// Index maintenance keeps a query answering the same rows as SQLite's.
#[test]
fn index_maintenance_matches_sqlite() {
    let compared = compare(
        "indexes",
        &[
            Step::Exec("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER)"),
            Step::Exec("INSERT INTO t VALUES(1, 'bbb', 30)"),
            Step::Exec("INSERT INTO t VALUES(2, 'aaa', 20)"),
            Step::Exec("INSERT INTO t VALUES(3, 'ccc', 10)"),
            Step::Exec("CREATE INDEX t_b ON t(b)"),
            Step::Exec("CREATE INDEX t_c ON t(c)"),
            Step::Exec("INSERT INTO t VALUES(4, 'aab', 40)"),
            Step::Query("SELECT a FROM t WHERE b = 'aaa'"),
            Step::Query("SELECT a FROM t WHERE b > 'aab' ORDER BY a"),
            Step::Exec("UPDATE t SET b = 'zzz' WHERE a = 2"),
            Step::Query("SELECT a FROM t WHERE b = 'aaa'"),
            Step::Query("SELECT a FROM t WHERE b = 'zzz'"),
            Step::Exec("DELETE FROM t WHERE c = 10"),
            Step::Query("SELECT a, b, c FROM t ORDER BY a"),
            Step::Exec("DROP INDEX t_c"),
            Step::Query("SELECT a FROM t WHERE c = 40"),
            Step::Exec("DROP TABLE t"),
            Step::Query("SELECT count(*) FROM sqlite_master WHERE name = 't'"),
        ],
    );
    assert!(compared == 0 || compared == 18, "compared {compared} steps");
}

/// The `sqlite_schema` rows rust-db writes are the ones SQLite writes.
#[test]
fn the_schema_table_matches_sqlite() {
    let compared = compare(
        "schema",
        &[
            Step::Exec("CREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT UNIQUE, note TEXT)"),
            Step::Exec("CREATE INDEX people_note ON people(note)"),
            Step::Exec("CREATE TABLE plain(a, b)"),
            Step::Exec("CREATE TABLE IF NOT EXISTS plain(a, b, c)"),
            Step::Query("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY name"),
            Step::Exec("DROP TABLE plain"),
            Step::Query("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY name"),
        ],
    );
    assert!(compared == 0 || compared == 7, "compared {compared} steps");
}

/// RETURNING reports the row each statement wrote.
#[test]
fn returning_matches_sqlite() {
    let compared = compare(
        "returning",
        &[
            Step::Exec("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)"),
            Step::Query("INSERT INTO t(b) VALUES('one') RETURNING a, b"),
            Step::Query("INSERT INTO t(b) VALUES('two') RETURNING a"),
            Step::Query("UPDATE t SET b = b || '!' RETURNING a, b"),
            Step::Query("DELETE FROM t WHERE a = 1 RETURNING a, b"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
        ],
    );
    assert!(compared == 0 || compared == 6, "compared {compared} steps");
}

/// UPSERT matches SQLite, in both its forms and with `excluded`.
#[test]
fn upsert_matches_sqlite() {
    let compared = compare(
        "upsert",
        &[
            Step::Exec("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT UNIQUE, hits INTEGER)"),
            Step::Exec("INSERT INTO t VALUES(1, 'one', 1)"),
            Step::Exec("INSERT INTO t VALUES(1, 'uno', 5) ON CONFLICT DO NOTHING"),
            Step::Query("SELECT a, b, hits FROM t ORDER BY a"),
            Step::Exec(
                "INSERT INTO t VALUES(1, 'uno', 5) ON CONFLICT(a) DO UPDATE SET hits = hits + excluded.hits",
            ),
            Step::Query("SELECT a, b, hits FROM t ORDER BY a"),
            Step::Exec(
                "INSERT INTO t VALUES(2, 'two', 7) ON CONFLICT(a) DO UPDATE SET hits = hits + excluded.hits",
            ),
            Step::Query("SELECT a, b, hits FROM t ORDER BY a"),
            Step::Exec(
                "INSERT INTO t VALUES(3, 'one', 9) ON CONFLICT(b) DO UPDATE SET hits = excluded.hits",
            ),
            Step::Query("SELECT a, b, hits FROM t ORDER BY a"),
            Step::Exec(
                "INSERT INTO t VALUES(1, 'one', 100) ON CONFLICT(a) DO UPDATE SET hits = excluded.hits WHERE hits < 0",
            ),
            Step::Query("SELECT a, b, hits FROM t ORDER BY a"),
            Step::Exec("INSERT INTO t VALUES(9, 'nine', 1) ON CONFLICT DO NOTHING"),
            Step::Query("SELECT a, b, hits FROM t ORDER BY a"),
        ],
    );
    assert!(compared == 0 || compared == 14, "compared {compared} steps");
}
