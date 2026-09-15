//! Foreign keys, graded against the pinned SQLite.
//!
//! Invariant: every claim here is the reference's answer, not this engine's.
//! A constraint is a refusal, and an engine that refuses slightly different
//! things from SQLite is one whose schemas do not port - so the interesting
//! cases are run against both and compared, rather than asserted from what
//! this implementation happens to do.
//!
//! The default is off, and the tests turn it on. That is SQLite's default too,
//! and it is the reason a foreign key is worth so little until somebody asks
//! for it: an existing database whose constraints have never been enforced
//! would start refusing writes the application has always made.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent_compat::facade::Database;
use inillucent_compat::interchange::reference_shell as pinned_shell;
use inillucent_compat::rendering::shell_text as render;
use inillucent_compat::workspace_root;

/// Returns a fresh scratch path, with every companion file removed.
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output/foreign-keys");
    let _ = std::fs::create_dir_all(&directory);
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(directory.join(format!("{name}.db{suffix}")));
    }
    directory.join(format!("{name}.db"))
}

/// Runs statements in the pinned shell, reporting stdout and stderr together.
///
/// The shell keeps going after an error - which is its default - so a script
/// whose third statement is refused still reports what the fourth one saw.
/// That is what makes a refusal comparable rather than merely fatal.
///
/// Rows come out on stdout and refusals on stderr, so the two arrive already
/// separated and the refusals land after the rows. [`normalise`] puts inillucent's
/// report in the same shape rather than pretending the shell interleaves them.
fn shell(path: &Path, script: &str) -> Option<String> {
    let program = pinned_shell()?;
    let output = Command::new(program).arg(path).arg(script).output().ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Some(text)
}

/// Opens a inillucent connection on a path.
fn connect(path: &Path) -> inillucent_compat::facade::Connection {
    let database = Database::open(path).expect("the database opens");
    database.session().expect("the connection opens")
}

/// Runs a script and returns what each statement reported, errors included.
///
/// The shape mirrors the shell's: one line per row, and a line beginning
/// `Error:` where a statement was refused. That is what makes the two
/// comparable at all.
fn run(connection: &inillucent_compat::facade::Connection, script: &str) -> String {
    let mut out = String::new();
    let mut rest = script;
    // The script is split by the parser rather than on semicolons: a
    // `CREATE TRIGGER` body has semicolons inside it, and splitting on them
    // turns one statement into three that do not parse.
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
///
/// Error *messages* are compared for the foreign-key ones, because SQLite's is
/// a fixed string that applications match on; anything else is compared as a
/// refusal, since prose is not a contract.
fn grade(name: &str, script: &str) {
    let Some(reference) = shell(&scratch(&format!("{name}-ref")), script) else {
        inillucent_compat::differential::skipping("the pinned shell is not present");
        return;
    };
    let candidate = run(&connect(&scratch(&format!("{name}-inillucent"))), script);
    let expected = normalise(&reference);
    let actual = normalise(&candidate);
    assert_eq!(
        expected, actual,
        "\n--- script\n{script}\n--- sqlite\n{reference}\n--- inillucent\n{candidate}"
    );
}

/// Reduces a report to what the two engines are required to agree on.
///
/// Two reductions, and both are about the shell rather than about the engines.
/// Every refusal becomes the constraint it names, because the rest of the
/// message is prose: "FOREIGN KEY constraint failed" is a string applications
/// match on and is compared exactly, and anything else is compared as a
/// refusal of that kind. And the refusals are moved to the end, in order,
/// because the shell writes rows to stdout and refusals to stderr - so it
/// cannot interleave them and neither engine's real ordering is observable
/// from outside. What each statement did is still pinned, by the `SELECT`s the
/// scripts end with.
fn normalise(report: &str) -> Vec<String> {
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for line in report.lines().map(|line| line.trim_end()) {
        if line.is_empty() {
            continue;
        }
        if !line.to_ascii_lowercase().contains("error") {
            rows.push(line.to_string());
            continue;
        }
        errors.push(if line.contains("FOREIGN KEY constraint failed") {
            "Error: FOREIGN KEY constraint failed".to_string()
        } else if line.contains("foreign key mismatch") {
            "Error: foreign key mismatch".to_string()
        } else if line.contains("no such table") {
            "Error: no such table".to_string()
        } else {
            "Error".to_string()
        });
    }
    rows.extend(errors);
    rows
}

/// The schema every test writes against: one parent, one child, and a child
/// with a two-column key.
const SCHEMA: &str = "PRAGMA foreign_keys=ON;
CREATE TABLE p(id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE c(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id));
INSERT INTO p VALUES (1,'one'),(2,'two');
";

/// A child row whose parent is missing is refused, and one whose parent is
/// there is not.
#[test]
fn a_child_needs_its_parent() {
    grade(
        "child-needs-parent",
        &format!(
            "{SCHEMA}
             INSERT INTO c VALUES (10, 1);
             INSERT INTO c VALUES (11, 99);
             INSERT INTO c VALUES (12, NULL);
             SELECT id, pid FROM c ORDER BY id;"
        ),
    );
}

/// With enforcement off, the same rows are accepted - which is the default and
/// the reason an existing database is not broken by upgrading.
#[test]
fn enforcement_is_off_until_it_is_asked_for() {
    grade(
        "off-by-default",
        "CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE c(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id));
         INSERT INTO c VALUES (1, 99);
         SELECT count(*) FROM c;
         PRAGMA foreign_keys;",
    );
}

/// An update that moves a child's key to a parent that is not there is refused
/// exactly as an insert would be.
#[test]
fn an_update_checks_the_new_key() {
    grade(
        "update-checks",
        &format!(
            "{SCHEMA}
             INSERT INTO c VALUES (10, 1);
             UPDATE c SET pid = 2 WHERE id = 10;
             UPDATE c SET pid = 99 WHERE id = 10;
             UPDATE c SET id = 11 WHERE id = 10;
             SELECT id, pid FROM c ORDER BY id;"
        ),
    );
}

/// Deleting a parent that still has children is refused by the default action,
/// and allowed once the children are gone.
#[test]
fn a_parent_with_children_cannot_be_deleted() {
    grade(
        "parent-with-children",
        &format!(
            "{SCHEMA}
             INSERT INTO c VALUES (10, 1);
             DELETE FROM p WHERE id = 1;
             DELETE FROM p WHERE id = 2;
             DELETE FROM c WHERE id = 10;
             DELETE FROM p WHERE id = 1;
             SELECT id FROM p ORDER BY id;"
        ),
    );
}

/// `ON DELETE CASCADE` takes the children with the parent.
#[test]
fn a_cascade_takes_the_children() {
    grade(
        "cascade-delete",
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE c(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id) ON DELETE CASCADE);
         INSERT INTO p VALUES (1),(2);
         INSERT INTO c VALUES (10,1),(11,1),(12,2);
         DELETE FROM p WHERE id = 1;
         SELECT id, pid FROM c ORDER BY id;
         SELECT count(*) FROM p;",
    );
}

/// `ON UPDATE CASCADE` moves the children's key with the parent's.
#[test]
fn a_cascade_moves_the_children() {
    grade(
        "cascade-update",
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE c(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id) ON UPDATE CASCADE);
         INSERT INTO p VALUES (1),(2);
         INSERT INTO c VALUES (10,1),(11,1);
         UPDATE p SET id = 7 WHERE id = 1;
         SELECT id, pid FROM c ORDER BY id;
         SELECT id FROM p ORDER BY id;",
    );
}

/// `SET NULL` and `SET DEFAULT` keep the children and let go of the key.
#[test]
fn set_null_and_set_default_keep_the_children() {
    grade(
        "set-null",
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE c(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id) ON DELETE SET NULL);
         CREATE TABLE d(id INTEGER PRIMARY KEY,
                        pid INTEGER DEFAULT 2 REFERENCES p(id) ON DELETE SET DEFAULT);
         INSERT INTO p VALUES (1),(2);
         INSERT INTO c VALUES (10,1);
         INSERT INTO d VALUES (20,1);
         DELETE FROM p WHERE id = 1;
         SELECT id, pid FROM c ORDER BY id;
         SELECT id, pid FROM d ORDER BY id;",
    );
}

/// `RESTRICT` refuses even when a `SET NULL` on the same statement would have
/// satisfied the constraint, because it fires before the write.
#[test]
fn restrict_refuses_where_no_action_would_not() {
    grade(
        "restrict",
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE c(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id) ON DELETE RESTRICT);
         INSERT INTO p VALUES (1);
         INSERT INTO c VALUES (10,1);
         DELETE FROM p WHERE id = 1;
         SELECT count(*) FROM p;",
    );
}

/// A two-column key is satisfied only when both columns match one parent row,
/// and a key with a NULL in it is not checked at all.
#[test]
fn a_composite_key_matches_on_every_column() {
    grade(
        "composite",
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(a INTEGER, b INTEGER, PRIMARY KEY(a,b));
         CREATE TABLE c(id INTEGER PRIMARY KEY, x INTEGER, y INTEGER,
                        FOREIGN KEY(x,y) REFERENCES p(a,b));
         INSERT INTO p VALUES (1,1),(1,2);
         INSERT INTO c VALUES (10,1,1);
         INSERT INTO c VALUES (11,1,3);
         INSERT INTO c VALUES (12,1,NULL);
         INSERT INTO c VALUES (13,NULL,NULL);
         SELECT id, x, y FROM c ORDER BY id;",
    );
}

/// A self-referencing tree cascades all the way down, not one level.
///
/// This is the case an inlined action has to be told about: the cascade reaches
/// the same table again, so the body has to exist once per level it can reach.
#[test]
fn a_self_referencing_cascade_reaches_the_leaves() {
    grade(
        "self-cascade",
        "PRAGMA foreign_keys=ON;
         CREATE TABLE node(id INTEGER PRIMARY KEY,
                           parent INTEGER REFERENCES node(id) ON DELETE CASCADE);
         INSERT INTO node VALUES (1,NULL),(2,1),(3,2),(4,3),(5,4),(6,1);
         DELETE FROM node WHERE id = 1;
         SELECT count(*) FROM node;",
    );
}

/// A key pointing at a table that is not there is legal to declare and fails
/// when something writes.
#[test]
fn a_missing_parent_fails_at_the_write() {
    grade(
        "missing-parent",
        "PRAGMA foreign_keys=ON;
         CREATE TABLE c(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES nowhere(id));
         INSERT INTO c VALUES (1, 1);
         SELECT count(*) FROM c;",
    );
}

/// A key whose parent columns are not a key of the parent is a mismatch, not a
/// violation: the constraint cannot be evaluated at all.
#[test]
fn a_parent_without_a_unique_key_is_a_mismatch() {
    grade(
        "mismatch",
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(id INTEGER, name TEXT);
         CREATE TABLE c(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id));
         INSERT INTO p VALUES (1,'one');
         INSERT INTO c VALUES (10,1);
         SELECT count(*) FROM c;",
    );
}

/// A unique index on the parent is as good as its primary key.
#[test]
fn a_unique_index_makes_a_parent_key() {
    grade(
        "unique-parent",
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(id INTEGER PRIMARY KEY, code TEXT);
         CREATE UNIQUE INDEX p_code ON p(code);
         CREATE TABLE c(id INTEGER PRIMARY KEY, code TEXT REFERENCES p(code));
         INSERT INTO p VALUES (1,'aa'),(2,'bb');
         INSERT INTO c VALUES (10,'aa');
         INSERT INTO c VALUES (11,'zz');
         SELECT id, code FROM c ORDER BY id;",
    );
}

/// A deferred key lets the transaction be inconsistent in the middle and
/// refuses the commit if it still is at the end.
#[test]
fn a_deferred_key_is_checked_at_the_commit() {
    grade(
        "deferred",
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE c(id INTEGER PRIMARY KEY,
                        pid INTEGER REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED);
         BEGIN;
         INSERT INTO c VALUES (10, 1);
         INSERT INTO p VALUES (1);
         COMMIT;
         SELECT id, pid FROM c ORDER BY id;",
    );
}

/// The same transaction, left inconsistent, cannot commit.
#[test]
fn a_deferred_key_refuses_a_commit_it_cannot_satisfy() {
    grade(
        "deferred-fails",
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE c(id INTEGER PRIMARY KEY,
                        pid INTEGER REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED);
         BEGIN;
         INSERT INTO c VALUES (10, 1);
         COMMIT;
         ROLLBACK;
         SELECT count(*) FROM c;",
    );
}

/// A trigger on the child sees the row the constraint let through, and a
/// trigger that writes a parent row satisfies the constraint for the rows
/// after it.
#[test]
fn triggers_and_keys_run_in_the_documented_order() {
    grade(
        "triggers",
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE c(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id));
         CREATE TABLE log(what TEXT);
         CREATE TRIGGER c_after AFTER INSERT ON c BEGIN INSERT INTO log VALUES ('inserted'); END;
         INSERT INTO p VALUES (1);
         INSERT INTO c VALUES (10,1);
         INSERT INTO c VALUES (11,99);
         SELECT what FROM log;
         SELECT count(*) FROM c;",
    );
}

/// `PRAGMA foreign_key_list` reports what the schema declared.
#[test]
fn the_key_list_reports_the_schema() {
    grade(
        "key-list",
        "CREATE TABLE p(a INTEGER, b INTEGER, PRIMARY KEY(a,b));
         CREATE TABLE c(id INTEGER PRIMARY KEY, x INTEGER, y INTEGER,
                        FOREIGN KEY(x,y) REFERENCES p(a,b) ON DELETE CASCADE ON UPDATE SET NULL);
         PRAGMA foreign_key_list(c);",
    );
}

/// `PRAGMA foreign_key_check` finds the rows a database written with
/// enforcement off left behind.
#[test]
fn the_key_check_finds_what_enforcement_would_have_refused() {
    grade(
        "key-check",
        "CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE c(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id));
         INSERT INTO p VALUES (1);
         INSERT INTO c VALUES (10,1),(11,99),(12,NULL);
         PRAGMA foreign_key_check;",
    );
}

/// A `REPLACE` that deletes a parent row to make room fires the key's action
/// on the way, which is the interaction between conflict handling and keys.
#[test]
fn replace_fires_the_action_on_the_row_it_removes() {
    grade(
        "replace",
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(id INTEGER PRIMARY KEY, name TEXT UNIQUE);
         CREATE TABLE c(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id) ON DELETE CASCADE);
         INSERT INTO p VALUES (1,'one');
         INSERT INTO c VALUES (10,1);
         INSERT OR REPLACE INTO p VALUES (2,'one');
         SELECT count(*) FROM c;
         SELECT id, name FROM p ORDER BY id;",
    );
}
