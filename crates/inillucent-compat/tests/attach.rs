//! `ATTACH`, name resolution across databases, and the commit that spans them.
//!
//! Invariant: a statement means the same database this engine means. A name
//! that resolves to `main` here and to `aux` in SQLite is a query that returns
//! different rows from the same schema, so the resolution order is graded
//! against the reference rather than asserted, and so is what happens when two
//! databases are written by one transaction.
//!
//! The crash cuts are the other half. A commit across two files is atomic
//! because one file's deletion decides both, and the way to test that claim is
//! to stop at each step and open what is left.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use inillucent_compat::facade::Database;
use inillucent_compat::workspace_root;
use inillucent_value::Value;

/// Returns the pinned SQLite shell, or `None` when it has not been downloaded.
fn pinned_shell() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_SHELL") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }
    let directory = workspace_root().join(".sqlite-ref/3.53.4/shell");
    let names: [&str; 2] = if cfg!(windows) {
        ["sqlite3.exe", "sqlite3"]
    } else {
        ["sqlite3", "sqlite3.exe"]
    };
    for name in names {
        let path = directory.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// Returns a scratch directory for one scenario, emptied first.
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output/attach").join(name);
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::create_dir_all(&directory);
    directory
}

/// Runs a script in the pinned shell.
fn shell(main: &Path, script: &str) -> Option<String> {
    let program = pinned_shell()?;
    let output = Command::new(program).arg(main).arg(script).output().ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Some(text)
}

/// Runs a script through inillucent, statement by statement.
fn run(main: &Path, script: &str) -> String {
    let database = Database::open(main).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
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

/// Renders one value the way the shell prints it.
fn render(value: &Value<'_>) -> String {
    match value {
        Value::Null => String::new(),
        Value::Integer(number) => number.to_string(),
        Value::Real(number) => format!("{number}"),
        Value::Text(text) => String::from_utf8_lossy(&text.utf8_bytes()).to_string(),
        Value::Blob(bytes) => String::from_utf8_lossy(bytes.raw()).to_string(),
    }
}

/// Runs one script against both engines and requires the same report.
///
/// Both run with the scratch directory as their working directory, so the
/// relative file name an `ATTACH` names resolves to the same file for each.
fn grade(name: &str, script: &str) {
    let reference_dir = scratch(&format!("{name}-ref"));
    let reference_main = reference_dir.join("main.db");
    let Some(reference) = shell(&reference_main, &resolved(script, &reference_dir)) else {
        inillucent_compat::differential::skipping("the pinned shell is not present");
        return;
    };
    let candidate_dir = scratch(&format!("{name}-inillucent"));
    let candidate_main = candidate_dir.join("main.db");
    let candidate = run(&candidate_main, &resolved(script, &candidate_dir));
    let script = resolved(script, &reference_dir);
    assert_eq!(
        normalise(&reference),
        normalise(&candidate),
        "\n--- script\n{script}\n--- sqlite\n{reference}\n--- inillucent\n{candidate}"
    );
}

/// Points every file name in a script at one engine's own directory.
///
/// A relative name would depend on the process's working directory, and these
/// tests run in parallel threads of one process - so there is exactly one of
/// those and it cannot belong to a test.
fn resolved(script: &str, directory: &Path) -> String {
    let prefix = directory.display().to_string().replace('\\', "/");
    script.replace("{dir}", &prefix)
}

/// Reduces a report to what the two engines must agree on: the rows in order,
/// then the refusals in order.
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

/// An attached database's tables are reachable by their qualified names.
#[test]
fn an_attached_database_is_reachable() {
    grade(
        "reachable",
        "CREATE TABLE t(a);
         INSERT INTO t VALUES (1);
         ATTACH DATABASE '{dir}/aux.db' AS aux;
         CREATE TABLE aux.t(a);
         INSERT INTO aux.t VALUES (2);
         SELECT a FROM main.t;
         SELECT a FROM aux.t;
         SELECT count(*) FROM t;",
    );
}

/// An unqualified name finds `main` before an attached database, whichever
/// order they were created in.
#[test]
fn main_wins_an_unqualified_name() {
    grade(
        "precedence",
        "ATTACH DATABASE '{dir}/aux.db' AS aux;
         CREATE TABLE aux.t(a);
         INSERT INTO aux.t VALUES (99);
         CREATE TABLE main.t(a);
         INSERT INTO main.t VALUES (1);
         SELECT a FROM t;
         SELECT a FROM aux.t;",
    );
}

/// A name only one database has resolves to that one.
#[test]
fn an_unqualified_name_finds_the_only_database_that_has_it() {
    grade(
        "only-one",
        "ATTACH DATABASE '{dir}/aux.db' AS aux;
         CREATE TABLE aux.only(a);
         INSERT INTO aux.only VALUES (7);
         SELECT a FROM only;",
    );
}

/// A join reads two databases in one statement.
#[test]
fn a_join_spans_two_databases() {
    grade(
        "join",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO t VALUES (1,'one'),(2,'two');
         ATTACH DATABASE '{dir}/aux.db' AS aux;
         CREATE TABLE aux.u(id INTEGER PRIMARY KEY, tag TEXT);
         INSERT INTO aux.u VALUES (1,'x'),(3,'z');
         SELECT t.id, t.name, aux.u.tag FROM t JOIN aux.u ON aux.u.id = t.id ORDER BY t.id;",
    );
}

/// One transaction writing two databases commits both or neither.
#[test]
fn a_transaction_spans_two_databases() {
    grade(
        "two-writers",
        "CREATE TABLE t(a);
         ATTACH DATABASE '{dir}/aux.db' AS aux;
         CREATE TABLE aux.t(a);
         BEGIN;
         INSERT INTO t VALUES (1);
         INSERT INTO aux.t VALUES (2);
         COMMIT;
         SELECT a FROM main.t;
         SELECT a FROM aux.t;",
    );
}

/// The same transaction rolled back leaves both databases alone.
#[test]
fn a_rollback_spans_two_databases() {
    grade(
        "two-rollback",
        "CREATE TABLE t(a);
         ATTACH DATABASE '{dir}/aux.db' AS aux;
         CREATE TABLE aux.t(a);
         INSERT INTO t VALUES (1);
         INSERT INTO aux.t VALUES (2);
         BEGIN;
         INSERT INTO t VALUES (10);
         INSERT INTO aux.t VALUES (20);
         ROLLBACK;
         SELECT a FROM main.t;
         SELECT a FROM aux.t;",
    );
}

/// A savepoint inside a two-database transaction undoes both sides.
#[test]
fn a_savepoint_spans_two_databases() {
    grade(
        "two-savepoint",
        "CREATE TABLE t(a);
         ATTACH DATABASE '{dir}/aux.db' AS aux;
         CREATE TABLE aux.t(a);
         BEGIN;
         INSERT INTO t VALUES (1);
         INSERT INTO aux.t VALUES (2);
         SAVEPOINT s;
         INSERT INTO t VALUES (10);
         INSERT INTO aux.t VALUES (20);
         ROLLBACK TO s;
         COMMIT;
         SELECT a FROM main.t ORDER BY a;
         SELECT a FROM aux.t ORDER BY a;",
    );
}

/// `main` is not a name another database can take.
#[test]
fn main_cannot_be_attached_over() {
    grade(
        "reserved-main",
        "ATTACH DATABASE '{dir}/aux.db' AS main; SELECT 'reached';",
    );
}

/// Nor is `temp`, which the connection has whether or not anything is in it.
#[test]
fn temp_cannot_be_attached_over() {
    grade(
        "reserved-temp",
        "ATTACH DATABASE '{dir}/aux.db' AS temp; SELECT 'reached';",
    );
}

/// A name in use is in use, whichever file the second one names.
#[test]
fn a_name_can_only_be_attached_once() {
    grade(
        "reserved-twice",
        "ATTACH DATABASE '{dir}/aux.db' AS aux;
         ATTACH DATABASE '{dir}/other.db' AS aux;
         SELECT 'reached';",
    );
}

/// `main` cannot be detached.
#[test]
fn main_cannot_be_detached() {
    grade("detach-main", "DETACH DATABASE main; SELECT 'reached';");
}

/// Nor can a name nobody attached.
#[test]
fn an_unattached_name_cannot_be_detached() {
    grade(
        "detach-missing",
        "DETACH DATABASE nosuch; SELECT 'reached';",
    );
}

/// Detaching twice fails the second time.
#[test]
fn detaching_twice_fails_the_second_time() {
    grade(
        "detach-twice",
        "ATTACH DATABASE '{dir}/aux.db' AS aux;
         DETACH DATABASE aux;
         DETACH DATABASE aux;
         SELECT 'reached';",
    );
}

/// A detached database's tables stop resolving.
#[test]
fn detaching_takes_the_names_with_it() {
    grade(
        "detach",
        "ATTACH DATABASE '{dir}/aux.db' AS aux;
         CREATE TABLE aux.gone(a);
         INSERT INTO aux.gone VALUES (1);
         SELECT count(*) FROM aux.gone;
         DETACH DATABASE aux;
         SELECT count(*) FROM aux.gone;",
    );
}

/// A database may join a transaction that is already open, which is what
/// SQLite allows and what makes `ATTACH` usable from inside a script.
#[test]
fn attaching_inside_a_transaction_is_allowed() {
    grade(
        "in-transaction",
        "CREATE TABLE t(a);
         BEGIN;
         ATTACH DATABASE '{dir}/aux.db' AS aux;
         INSERT INTO t VALUES (1);
         COMMIT;
         SELECT a FROM t;",
    );
}

/// A two-database commit leaves no super-journal behind, and both files carry
/// the transaction.
#[test]
fn a_two_database_commit_cleans_up_after_itself() {
    let directory = scratch("cleanup");
    let main = directory.join("main.db");
    let aux = directory.join("aux.db");
    {
        let database = Database::open(&main).expect("the database opens");
        let connection = database.connect().expect("the connection opens");
        connection
            .execute_batch(&format!(
                "CREATE TABLE t(a);
                 ATTACH DATABASE '{}' AS aux;
                 CREATE TABLE aux.t(a);
                 BEGIN;
                 INSERT INTO t VALUES (1);
                 INSERT INTO aux.t VALUES (2);
                 COMMIT;",
                aux.display().to_string().replace('\\', "/")
            ))
            .expect("the transaction commits");
    }
    let leftovers: Vec<String> = std::fs::read_dir(&directory)
        .expect("the directory is readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains("-mj") || name.ends_with("-journal"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "the commit left files behind: {leftovers:?}"
    );

    let database = Database::open(&main).expect("the database reopens");
    let connection = database.connect().expect("the connection opens");
    connection
        .execute_batch(&format!(
            "ATTACH DATABASE '{}' AS aux",
            aux.display().to_string().replace('\\', "/")
        ))
        .expect("the second database attaches");
    let rows = connection
        .query("SELECT (SELECT count(*) FROM main.t), (SELECT count(*) FROM aux.t)")
        .expect("the query runs");
    let counts: Vec<i64> = rows
        .first()
        .map(|row| row.iter().filter_map(Value::as_integer).collect())
        .unwrap_or_default();
    assert_eq!(counts, vec![1, 1], "both databases must carry the commit");
}

/// Two databases share one page cache only by accident of number, so a page
/// two in one is never served for a page two in the other.
#[test]
fn the_same_page_number_in_two_databases_is_two_pages() {
    let directory = scratch("page-numbers");
    let main = directory.join("main.db");
    let aux = directory.join("aux.db");
    let database = Database::open(&main).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    connection
        .execute_batch(&format!(
            "CREATE TABLE t(a TEXT);
             INSERT INTO t VALUES ('from main');
             ATTACH DATABASE '{}' AS aux;
             CREATE TABLE aux.t(a TEXT);
             INSERT INTO aux.t VALUES ('from aux');",
            aux.display().to_string().replace('\\', "/")
        ))
        .expect("both databases are written");
    let rows = connection
        .query("SELECT (SELECT a FROM main.t), (SELECT a FROM aux.t)")
        .expect("the query runs");
    let cells: Vec<String> = rows
        .first()
        .map(|row| row.iter().map(render).collect())
        .unwrap_or_default();
    assert_eq!(cells, vec!["from main".to_string(), "from aux".to_string()]);
    let _ = Arc::new(());
}
