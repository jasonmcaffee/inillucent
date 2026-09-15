//! Subqueries in the places a write puts them, graded against the reference.
//!
//! Invariant: a subquery is a subquery wherever it is written. Phase 8 made
//! them work in every position a `SELECT` has; a `DELETE`'s `WHERE` is the
//! same position with a different statement around it, and until phase 9 it
//! compiled a probe against a store nobody had opened - so `DELETE FROM t
//! WHERE id IN (SELECT ...)`, which is one of the most ordinary statements
//! there is, answered `SQLITE_MISUSE`.
//!
//! The defect was found by a foreign key: a cyclic action is applied by a
//! statement of exactly that shape, and it could not run.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent_compat::facade::Database;
use inillucent_compat::interchange::reference_shell as pinned_shell;
use inillucent_compat::rendering::shell_text as render;
use inillucent_compat::workspace_root;

/// Returns a fresh scratch path.
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output/dml-subqueries");
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

/// Runs a script through inillucent, statement by statement.
fn run(path: &Path, script: &str) -> String {
    let database = Database::open(path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
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

/// Runs a script against both engines and requires the same rows.
fn grade(name: &str, script: &str) {
    let Some(reference) = shell(&scratch(&format!("{name}-ref")), script) else {
        inillucent_compat::differential::skipping("the pinned shell is not present");
        return;
    };
    let candidate = run(&scratch(&format!("{name}-inillucent")), script);
    let expected: Vec<&str> = reference.lines().filter(|line| !line.is_empty()).collect();
    let actual: Vec<&str> = candidate.lines().filter(|line| !line.is_empty()).collect();
    assert_eq!(
        expected, actual,
        "\n--- script\n{script}\n--- sqlite\n{reference}\n--- inillucent\n{candidate}"
    );
}

/// The schema the tests write against.
const SCHEMA: &str = "CREATE TABLE a(id INTEGER PRIMARY KEY, tag TEXT);
CREATE TABLE b(id INTEGER, tag TEXT);
INSERT INTO a VALUES (1,'x'),(2,'y'),(3,'z'),(4,'x');
INSERT INTO b VALUES (1,'x'),(3,'q');
";

/// `DELETE ... WHERE id IN (SELECT ...)`.
#[test]
fn a_delete_can_test_a_set() {
    grade(
        "delete-in",
        &format!(
            "{SCHEMA} DELETE FROM a WHERE id IN (SELECT id FROM b); SELECT id FROM a ORDER BY id;"
        ),
    );
}

/// `DELETE ... WHERE EXISTS (correlated)`.
#[test]
fn a_delete_can_test_a_correlated_existence() {
    grade(
        "delete-exists",
        &format!(
            "{SCHEMA} DELETE FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.tag = a.tag);
             SELECT id FROM a ORDER BY id;"
        ),
    );
}

/// `DELETE ... WHERE NOT EXISTS (correlated)`, which is the shape a foreign
/// key's sweep uses.
#[test]
fn a_delete_can_test_a_correlated_absence() {
    grade(
        "delete-not-exists",
        &format!(
            "{SCHEMA} DELETE FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.id = a.id);
             SELECT id FROM a ORDER BY id;"
        ),
    );
}

/// `UPDATE ... SET x = (SELECT ...)` and `WHERE ... IN (SELECT ...)`.
#[test]
fn an_update_can_read_and_test_a_subquery() {
    grade(
        "update-subquery",
        &format!(
            "{SCHEMA} UPDATE a SET tag = (SELECT tag FROM b WHERE b.id = a.id)
             WHERE id IN (SELECT id FROM b);
             SELECT id, tag FROM a ORDER BY id;"
        ),
    );
}

/// `INSERT ... VALUES ((SELECT ...))`.
#[test]
fn an_insert_can_read_a_subquery() {
    grade(
        "insert-subquery",
        &format!(
            "{SCHEMA} INSERT INTO a VALUES (9, (SELECT tag FROM b WHERE id = 3));
             SELECT id, tag FROM a ORDER BY id;"
        ),
    );
}

/// A subquery in a `RETURNING` clause is evaluated per row returned.
#[test]
fn a_returning_clause_can_read_a_subquery() {
    grade(
        "returning-subquery",
        &format!(
            "{SCHEMA} DELETE FROM a WHERE id = 1
             RETURNING id, (SELECT count(*) FROM b);
             SELECT count(*) FROM a;"
        ),
    );
}
