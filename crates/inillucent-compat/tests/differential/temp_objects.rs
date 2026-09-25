//! Temporary tables, views, indexes and triggers.
//!
//! Invariant: a temporary object lives in the connection's own database and
//! nowhere else. It shadows a permanent object of the same name, it is gone
//! when the connection is, and nothing about it reaches the file the
//! connection was opened on - which is what makes it safe to create one on a
//! database somebody else is reading.
//!
//! Phase 8 refused them, and said why: they need a connection that can hold
//! more than one database, which is what `ATTACH` brought.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent_compat::facade::Database;
use inillucent_compat::interchange::reference_shell as pinned_shell;
use inillucent_compat::rendering::shell_text as render;
use inillucent_compat::workspace_root;
use inillucent_value::Value;

/// Returns a fresh scratch path for one scenario.
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output/temp");
    let _ = std::fs::create_dir_all(&directory);
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(directory.join(format!("{name}.db{suffix}")));
    }
    directory.join(format!("{name}.db"))
}

/// Runs a script in the pinned shell.
fn shell(path: &Path, script: &str) -> Option<String> {
    let program = pinned_shell()?;
    let output = Command::new(program).arg(path).arg(script).output().ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Some(text)
}

/// Runs a script through inillucent on one connection, statement by statement.
fn run(path: &Path, script: &str) -> String {
    let database = Database::open(path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    report(&connection, script)
}

/// Runs a script on an open connection and reports what it said.
fn report(connection: &inillucent_compat::facade::Connection, script: &str) -> String {
    let mut out = String::new();
    let mut rest = script;
    while !rest.trim().is_empty() {
        let prepared = connection.prepare_with_tail(rest);
        let consumed = match &prepared {
            Ok((_, consumed)) => *consumed,
            Err(_) => rest.len(),
        };
        match prepared {
            Ok((mut statement, _)) => loop {
                match statement.step() {
                    Ok(true) => {
                        let cells: Vec<String> = statement.row().iter().map(render).collect();
                        out.push_str(&cells.join("|"));
                        out.push('\n');
                    }
                    Ok(false) => break,
                    Err(error) => {
                        out.push_str("Error: ");
                        out.push_str(error.message());
                        out.push('\n');
                        break;
                    }
                }
            },
            Err(error) => {
                out.push_str("Error: ");
                out.push_str(error.message());
                out.push('\n');
            }
        }
        let Some(tail) = rest.get(consumed..) else {
            break;
        };
        if tail.len() >= rest.len() {
            break;
        }
        rest = tail;
    }
    out
}

/// Runs one script against both engines and requires the same report.
fn grade(name: &str, script: &str) {
    let Some(reference) = shell(&scratch(&format!("{name}-ref")), script) else {
        inillucent_compat::differential::skipping("the pinned shell is not present");
        return;
    };
    let candidate = run(&scratch(&format!("{name}-inillucent")), script);
    assert_eq!(
        normalise(&reference),
        normalise(&candidate),
        "\n--- script\n{script}\n--- sqlite\n{reference}\n--- inillucent\n{candidate}"
    );
}

/// Reduces a report to the rows in order, then the refusals in order.
fn normalise(report: &str) -> Vec<String> {
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for line in report.lines().map(|line| line.trim_end()) {
        if line.is_empty() {
            continue;
        }
        if line.to_ascii_lowercase().contains("error") {
            errors.push("Error".to_string());
        } else {
            rows.push(line.to_string());
        }
    }
    rows.extend(errors);
    rows
}

/// A temporary table holds rows and is reachable by name and by `temp.name`.
#[test]
fn a_temporary_table_holds_rows() {
    grade(
        "table",
        "CREATE TEMP TABLE scratch(a INTEGER PRIMARY KEY, b TEXT);
         INSERT INTO scratch VALUES (1,'one'),(2,'two');
         SELECT a, b FROM scratch ORDER BY a;
         SELECT count(*) FROM temp.scratch;",
    );
}

/// `TEMPORARY` is the same word written out.
#[test]
fn temporary_is_the_same_as_temp() {
    grade(
        "temporary",
        "CREATE TEMPORARY TABLE scratch(a);
         INSERT INTO scratch VALUES (1);
         SELECT count(*) FROM scratch;",
    );
}

/// A temporary table shadows a permanent one of the same name, and the
/// permanent one is still there under its own qualifier.
#[test]
fn a_temporary_table_shadows_a_permanent_one() {
    grade(
        "shadow",
        "CREATE TABLE t(a);
         INSERT INTO t VALUES (1);
         CREATE TEMP TABLE t(a);
         INSERT INTO t VALUES (99);
         SELECT a FROM t;
         SELECT a FROM main.t;
         SELECT a FROM temp.t;",
    );
}

/// Nothing about a temporary table reaches the file.
#[test]
fn a_temporary_table_is_not_in_the_file() {
    grade(
        "not-in-file",
        "CREATE TABLE kept(a);
         CREATE TEMP TABLE scratch(a);
         SELECT name FROM sqlite_schema ORDER BY name;
         SELECT count(*) FROM sqlite_temp_schema;",
    );
}

/// A temporary table is gone when the connection is.
#[test]
fn a_temporary_table_does_not_survive_the_connection() {
    let path = scratch("lifetime");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session().expect("the connection opens");
        connection
            .execute_batch("CREATE TEMP TABLE scratch(a); INSERT INTO scratch VALUES (1)")
            .expect("the temporary table is written");
        let rows = connection
            .query("SELECT count(*) FROM scratch")
            .expect("the query runs");
        assert_eq!(
            rows.first()
                .and_then(|row| row.first())
                .and_then(Value::as_integer),
            Some(1)
        );
    }
    let database = Database::open(&path).expect("the database reopens");
    let connection = database.session().expect("the connection opens");
    assert!(
        connection.query("SELECT count(*) FROM scratch").is_err(),
        "the temporary table outlived its connection"
    );
}

/// Two connections have a temporary database each.
#[test]
fn each_connection_has_its_own_temporary_database() {
    let path = scratch("per-connection");
    let database = Database::open(&path).expect("the database opens");
    let first = database.session().expect("the first connection opens");
    let second = database.session().expect("the second connection opens");
    first
        .execute_batch("CREATE TEMP TABLE scratch(a); INSERT INTO scratch VALUES (1)")
        .expect("the first writes its own");
    assert!(
        second.query("SELECT count(*) FROM scratch").is_err(),
        "one connection saw another's temporary table"
    );
    second
        .execute_batch("CREATE TEMP TABLE scratch(a); INSERT INTO scratch VALUES (2),(3)")
        .expect("the second writes its own");
    let count = |connection: &inillucent_compat::facade::Connection| {
        connection
            .query("SELECT count(*) FROM scratch")
            .expect("the query runs")
            .first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer)
    };
    assert_eq!(count(&first), Some(1));
    assert_eq!(count(&second), Some(2));
}

/// A temporary object's name may not be qualified.
#[test]
fn a_temporary_name_may_not_be_qualified() {
    grade(
        "qualified",
        "CREATE TEMP TABLE main.t(a); SELECT 'reached';",
    );
}

/// A temporary index over a temporary table is used like any other.
#[test]
fn a_temporary_index_works() {
    grade(
        "index",
        "CREATE TEMP TABLE scratch(a, b);
         CREATE INDEX temp.scratch_a ON scratch(a);
         INSERT INTO scratch VALUES (1,'one'),(2,'two'),(1,'uno');
         SELECT b FROM scratch WHERE a = 1 ORDER BY b;",
    );
}

/// A temporary view reads whatever it was written over.
#[test]
fn a_temporary_view_works() {
    grade(
        "view",
        "CREATE TABLE t(a);
         INSERT INTO t VALUES (1),(2),(3);
         CREATE TEMP VIEW big AS SELECT a FROM t WHERE a >= 2;
         SELECT a FROM big ORDER BY a;",
    );
}

/// A temporary trigger fires on a permanent table.
#[test]
fn a_temporary_trigger_fires() {
    grade(
        "trigger",
        "CREATE TABLE t(a);
         CREATE TEMP TABLE log(what);
         CREATE TEMP TRIGGER t_log AFTER INSERT ON t BEGIN INSERT INTO log VALUES (NEW.a); END;
         INSERT INTO t VALUES (1),(2);
         SELECT what FROM log ORDER BY what;",
    );
}

/// A transaction that writes both a temporary and a permanent table commits
/// both, and rolling it back undoes both.
#[test]
fn a_transaction_covers_the_temporary_database_too() {
    grade(
        "transaction",
        "CREATE TABLE t(a);
         CREATE TEMP TABLE scratch(a);
         BEGIN;
         INSERT INTO t VALUES (1);
         INSERT INTO scratch VALUES (1);
         COMMIT;
         BEGIN;
         INSERT INTO t VALUES (2);
         INSERT INTO scratch VALUES (2);
         ROLLBACK;
         SELECT count(*) FROM t;
         SELECT count(*) FROM scratch;",
    );
}

/// Dropping a temporary table leaves the permanent one it was shadowing.
#[test]
fn dropping_a_temporary_table_uncovers_the_permanent_one() {
    grade(
        "drop",
        "CREATE TABLE t(a);
         INSERT INTO t VALUES (1);
         CREATE TEMP TABLE t(a);
         INSERT INTO t VALUES (99);
         SELECT a FROM t;
         DROP TABLE temp.t;
         SELECT a FROM t;",
    );
}
