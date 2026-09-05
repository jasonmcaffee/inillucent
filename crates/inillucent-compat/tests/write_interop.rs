//! Files each engine writes, opened and judged by the other.
//!
//! Invariant: inillucent's work is judged by SQLite and SQLite's by inillucent, never
//! by the engine that produced it. A reader that accepts only what its own
//! writer produces has tested nothing, because the two agree by being the same
//! code.
//!
//! What is deliberately not claimed is byte-for-byte identity. Two valid
//! B-tree layouts differ - a different split point is not a different database
//! - so what has to match is the logical contents, `PRAGMA integrity_check`,
//! and the ability of each engine to keep writing after the other one has.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent::{Database, Value};
use inillucent_compat::workspace_root;

/// Returns the pinned SQLite shell, or `None` when it has not been downloaded.
fn pinned_shell() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_SHELL") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }
    let directory = workspace_root().join(".sqlite-ref/3.53.4/shell");
    // The name to try first is the one this platform runs. Both are on disk
    // when the workspace is shared between Windows and WSL, and a Linux process
    // that picks the `.exe` gets a *Windows* SQLite through binfmt interop -
    // which cannot open a Linux path, and says "unable to open database" for a
    // reason that has nothing to do with the database.
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

/// Returns a fresh scratch path for one scenario.
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output/task-1786/interop");
    let _ = std::fs::create_dir_all(&directory);
    for suffix in ["", "-journal"] {
        let _ = std::fs::remove_file(directory.join(format!("{name}.db{suffix}")));
    }
    directory.join(format!("{name}.db"))
}

/// Runs statements in the pinned shell and returns its stdout.
fn shell(path: &Path, statements: &[&str]) -> Option<String> {
    let program = pinned_shell()?;
    let mut script = String::new();
    for statement in statements {
        script.push_str(statement);
        script.push_str(";\n");
    }
    let output = Command::new(program).arg(path).arg(&script).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let errors = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "the pinned shell refused {script}\n{errors}"
    );
    Some(text)
}

/// Runs a script through inillucent.
fn inillucent(path: &Path, sql: &str) {
    let database = Database::open(path).expect("inillucent opens the file");
    let connection = database.connect().expect("inillucent connects");
    connection
        .execute_batch(sql)
        .expect("inillucent runs the script");
}

/// Returns the rows a inillucent query produces, rendered.
fn query(path: &Path, sql: &str) -> Vec<String> {
    let database = Database::open(path).expect("inillucent opens the file");
    let connection = database.connect().expect("inillucent connects");
    connection
        .query(sql)
        .expect("inillucent queries")
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    Value::Null => String::new(),
                    Value::Integer(integer) => integer.to_string(),
                    Value::Real(real) => format!("{real}"),
                    Value::Text(text) => String::from_utf8_lossy(&text.utf8_bytes()).into_owned(),
                    Value::Blob(blob) => blob
                        .raw()
                        .iter()
                        .map(|byte| format!("{byte:02X}"))
                        .collect::<Vec<String>>()
                        .join(""),
                })
                .collect::<Vec<String>>()
                .join("|")
        })
        .collect()
}

/// Announces a skipped run, once, in the words the harness uses elsewhere.
fn announce_skip() {
    eprintln!("the pinned SQLite shell is not built; run tools/sqlite-reference.{{ps1,sh}}");
}

/// A database inillucent built from nothing is one SQLite reads and approves of.
#[test]
fn sqlite_reads_what_inillucent_wrote() {
    if pinned_shell().is_none() {
        announce_skip();
        return;
    }
    let path = scratch("inillucent-wrote");
    inillucent(
        &path,
        "CREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT UNIQUE, score REAL, note TEXT);
         CREATE INDEX people_score ON people(score);
         INSERT INTO people VALUES(1, 'ada', 9.5, 'first');
         INSERT INTO people VALUES(2, 'grace', 8.25, NULL);
         INSERT INTO people(name, score, note) VALUES('alan', 7.0, 'third');
         UPDATE people SET score = score + 1 WHERE id = 2;
         DELETE FROM people WHERE name = 'alan';
         INSERT INTO people VALUES(10, 'linus', 1.5, 'tenth');",
    );

    let integrity = shell(&path, &["PRAGMA integrity_check"]).expect("the shell runs");
    assert_eq!(integrity.trim(), "ok", "SQLite found: {integrity}");

    let rows = shell(
        &path,
        &["SELECT id, name, score, note FROM people ORDER BY id"],
    )
    .expect("the shell runs");
    let seen: Vec<&str> = rows.lines().collect();
    assert_eq!(
        seen,
        vec!["1|ada|9.5|first", "2|grace|9.25|", "10|linus|1.5|tenth"],
        "SQLite read {seen:?}"
    );

    // The index inillucent built is one SQLite uses and agrees with.
    let by_index =
        shell(&path, &["SELECT name FROM people WHERE score = 9.25"]).expect("the shell runs");
    assert_eq!(by_index.trim(), "grace");

    let schema = shell(
        &path,
        &["SELECT type, name, sql FROM sqlite_master ORDER BY name"],
    )
    .expect("the shell runs");
    assert!(
        schema.contains(
            "CREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT UNIQUE, score REAL, note TEXT)"
        ),
        "the stored CREATE text is not what SQLite expects: {schema}"
    );
    assert!(
        schema.contains("sqlite_autoindex_people_1"),
        "the automatic index for the UNIQUE constraint is missing: {schema}"
    );
}

/// SQLite can keep writing to a database inillucent wrote, and the result is one
/// inillucent still reads.
#[test]
fn sqlite_writes_on_top_of_inillucent_and_inillucent_reads_it_back() {
    if pinned_shell().is_none() {
        announce_skip();
        return;
    }
    let path = scratch("both-wrote");
    inillucent(
        &path,
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER);
         CREATE INDEX t_b ON t(b);
         INSERT INTO t VALUES(1, 'one', 10);
         INSERT INTO t VALUES(2, 'two', 20);",
    );
    shell(
        &path,
        &[
            "INSERT INTO t VALUES(3, 'three', 30)",
            "UPDATE t SET c = c * 2 WHERE a = 1",
            "DELETE FROM t WHERE a = 2",
        ],
    )
    .expect("the shell runs");

    let rows = query(&path, "SELECT a, b, c FROM t ORDER BY a");
    assert_eq!(
        rows,
        vec!["1|one|20", "3|three|30"],
        "inillucent read {rows:?}"
    );

    // The index SQLite maintained is one inillucent seeks through.
    let by_index = query(&path, "SELECT a FROM t WHERE b = 'three'");
    assert_eq!(by_index, vec!["3"]);

    // And inillucent can keep writing after SQLite has.
    inillucent(
        &path,
        "INSERT INTO t VALUES(4, 'four', 40); DELETE FROM t WHERE a = 1;",
    );
    let integrity = shell(&path, &["PRAGMA integrity_check"]).expect("the shell runs");
    assert_eq!(integrity.trim(), "ok", "SQLite found: {integrity}");
    let rows = query(&path, "SELECT a, b, c FROM t ORDER BY a");
    assert_eq!(
        rows,
        vec!["3|three|30", "4|four|40"],
        "inillucent read {rows:?}"
    );
}

/// A database SQLite built is one inillucent writes into, and SQLite still
/// approves of the result.
#[test]
fn inillucent_writes_into_a_database_sqlite_built() {
    if pinned_shell().is_none() {
        announce_skip();
        return;
    }
    let path = scratch("sqlite-built");
    shell(
        &path,
        &[
            "CREATE TABLE stock(sku TEXT PRIMARY KEY, name TEXT, quantity INTEGER NOT NULL)",
            "CREATE INDEX stock_name ON stock(name)",
            "INSERT INTO stock VALUES('a1', 'widget', 4)",
            "INSERT INTO stock VALUES('b2', 'gadget', 9)",
        ],
    )
    .expect("the shell runs");

    inillucent(
        &path,
        "INSERT INTO stock VALUES('c3', 'sprocket', 2);
         UPDATE stock SET quantity = quantity - 1 WHERE sku = 'b2';
         DELETE FROM stock WHERE sku = 'a1';",
    );

    let integrity = shell(&path, &["PRAGMA integrity_check"]).expect("the shell runs");
    assert_eq!(integrity.trim(), "ok", "SQLite found: {integrity}");
    let rows = shell(
        &path,
        &["SELECT sku, name, quantity FROM stock ORDER BY sku"],
    )
    .expect("the shell runs");
    let seen: Vec<&str> = rows.lines().collect();
    assert_eq!(
        seen,
        vec!["b2|gadget|8", "c3|sprocket|2"],
        "SQLite read {seen:?}"
    );

    // The `sku` primary key is an automatic index SQLite created, and inillucent
    // maintained it: a lookup through it finds the row it wrote.
    let by_key =
        shell(&path, &["SELECT name FROM stock WHERE sku = 'c3'"]).expect("the shell runs");
    assert_eq!(by_key.trim(), "sprocket");
    let by_index =
        shell(&path, &["SELECT sku FROM stock WHERE name = 'sprocket'"]).expect("the shell runs");
    assert_eq!(by_index.trim(), "c3");
}

/// A table inillucent created and dropped leaves a database SQLite still accepts.
#[test]
fn dropping_a_table_leaves_a_database_sqlite_accepts() {
    if pinned_shell().is_none() {
        announce_skip();
        return;
    }
    let path = scratch("dropped");
    inillucent(
        &path,
        "CREATE TABLE keep(a INTEGER PRIMARY KEY, b TEXT);
         CREATE TABLE go(a INTEGER PRIMARY KEY, b TEXT UNIQUE, c TEXT);
         CREATE INDEX go_c ON go(c);
         INSERT INTO keep VALUES(1, 'kept');
         INSERT INTO go VALUES(1, 'x', 'y');
         INSERT INTO go VALUES(2, 'p', 'q');
         DROP TABLE go;
         INSERT INTO keep VALUES(2, 'also kept');",
    );
    let integrity = shell(&path, &["PRAGMA integrity_check"]).expect("the shell runs");
    assert_eq!(integrity.trim(), "ok", "SQLite found: {integrity}");
    let schema =
        shell(&path, &["SELECT name FROM sqlite_master ORDER BY name"]).expect("the shell runs");
    let names: Vec<&str> = schema.lines().collect();
    assert_eq!(
        names,
        vec!["keep"],
        "the dropped table left something behind: {names:?}"
    );
    let rows = shell(&path, &["SELECT a, b FROM keep ORDER BY a"]).expect("the shell runs");
    assert_eq!(
        rows.lines().collect::<Vec<&str>>(),
        vec!["1|kept", "2|also kept"]
    );
}

/// A transaction inillucent rolled back leaves nothing SQLite can see.
#[test]
fn a_rolled_back_transaction_is_invisible_to_sqlite() {
    if pinned_shell().is_none() {
        announce_skip();
        return;
    }
    let path = scratch("rolled-back");
    inillucent(
        &path,
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
         INSERT INTO t VALUES(1, 'kept');
         BEGIN;
         INSERT INTO t VALUES(2, 'gone');
         DELETE FROM t WHERE a = 1;
         ROLLBACK;",
    );
    assert!(
        !path
            .with_file_name(format!(
                "{}-journal",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("")
            ))
            .exists(),
        "a rolled back transaction left its journal behind"
    );
    let integrity = shell(&path, &["PRAGMA integrity_check"]).expect("the shell runs");
    assert_eq!(integrity.trim(), "ok", "SQLite found: {integrity}");
    let rows = shell(&path, &["SELECT a, b FROM t ORDER BY a"]).expect("the shell runs");
    assert_eq!(rows.lines().collect::<Vec<&str>>(), vec!["1|kept"]);
}
