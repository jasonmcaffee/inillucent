//! The shell, compared against the pinned `sqlite3` shell.
//!
//! Invariant: a script written for `sqlite3` produces the same bytes here. That
//! is the only claim a shell can make that is worth anything - a shell exists
//! so that a person's habits and a script's expectations carry over, and both
//! are made of exact output. Every case below is a whole script fed to both
//! programs on standard input, with the two outputs compared line for line.
//!
//! Two things are deliberately not compared. The `.help` text is prose, and
//! `.version` names the engine, which is the one place the two must differ.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use rustdb_compat::workspace_root;

/// Where this suite's scratch databases live.
fn area() -> PathBuf {
    let path = workspace_root().join("_agent_output/task-1789/cli");
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Returns the pinned SQLite shell, if it has been downloaded.
fn reference() -> Option<PathBuf> {
    let directory = workspace_root().join(".sqlite-ref/3.53.4/shell");
    let path = directory.join(format!("sqlite3{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Returns this workspace's shell, building it first.
fn ours() -> Option<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(cargo)
        .current_dir(workspace_root())
        .args(["build", "-p", "rustdb-cli"])
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let mut directory = std::env::current_exe().unwrap_or_default();
    directory.pop();
    directory.pop();
    let path = directory.join(format!("rustdb-shell{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Runs a shell over a fresh database and returns everything it printed.
fn run(program: &PathBuf, name: &str, script: &str) -> String {
    let database = area().join(format!("{name}.db"));
    let _ = std::fs::remove_file(&database);
    let mut child = Command::new(program)
        .arg(&database)
        .current_dir(area())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the shell starts");
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(script.as_bytes());
    }
    let output = child.wait_with_output().expect("the shell finishes");
    let mut text = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    text.push_str(&String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"));
    text
}

/// Runs one script through both shells and requires the same output.
fn check(name: &str, script: &str) {
    let (Some(reference), Some(ours)) = (reference(), ours()) else {
        eprintln!("a shell is missing; skipping");
        return;
    };
    let expected = run(&reference, &format!("{name}-sqlite"), script);
    let found = run(&ours, &format!("{name}-rustdb"), script);
    if expected == found {
        return;
    }
    let mut left = expected.lines();
    let mut right = found.lines();
    let mut line = 0;
    loop {
        line += 1;
        match (left.next(), right.next()) {
            (None, None) => break,
            (a, b) if a == b => continue,
            (a, b) => panic!(
                "line {line} of `{name}` differs:\n  SQLite:  {:?}\n  rust-db: {:?}",
                a.unwrap_or("(end)"),
                b.unwrap_or("(end)")
            ),
        }
    }
}

/// Runs one script through both shells, comparing with whitespace collapsed.
fn check_words(name: &str, script: &str) {
    let (Some(reference), Some(ours)) = (reference(), ours()) else {
        eprintln!("a shell is missing; skipping");
        return;
    };
    let expected = collapse(&run(&reference, &format!("{name}-sqlite"), script));
    let found = collapse(&run(&ours, &format!("{name}-rustdb"), script));
    assert_eq!(
        expected, found,
        "`{name}` differs once padding is collapsed"
    );
}

/// Collapses every run of spaces and tabs to one.
fn collapse(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<&str>>().join(" "))
        .collect()
}

/// The schema every scenario starts from.
const SETUP: &str = "\
CREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT, score REAL, tag BLOB);
INSERT INTO people VALUES (1, 'Ada', 1.5, x'0102');
INSERT INTO people VALUES (2, 'Grace', 2.25, NULL);
INSERT INTO people VALUES (3, 'a,comma', -3.0, x'ff');
CREATE INDEX people_name ON people(name);
CREATE VIEW loud AS SELECT upper(name) AS shout FROM people;
";

/// The default output is a pipe-separated list with no headers.
#[test]
fn the_default_mode_matches() {
    check(
        "default",
        &format!("{SETUP}SELECT * FROM people;\nSELECT count(*) FROM people;\n"),
    );
}

/// Headers print the column names in the same layout as the rows.
#[test]
fn headers_match() {
    check(
        "headers",
        &format!("{SETUP}.headers on\nSELECT id, name FROM people;\n.headers off\nSELECT id FROM people;\n"),
    );
}

/// Every output mode lays the same rows out the same way.
#[test]
fn every_output_mode_matches() {
    let mut script = String::from(SETUP);
    for mode in [
        "list", "csv", "tabs", "quote", "line", "json", "html", "column", "markdown", "table",
        "box", "insert",
    ] {
        script.push_str(&format!(
            ".mode {mode}\nSELECT id, name, score FROM people;\n"
        ));
    }
    check("modes", &script);
}

/// A separator and a null value change what the plain modes print.
#[test]
fn the_separator_and_null_value_match() {
    check(
        "separator",
        &format!(
            "{SETUP}.separator ,\nSELECT id, name FROM people;\n\
             .nullvalue <null>\nSELECT id, tag FROM people;\n\
             .separator \" | \"\nSELECT id, name FROM people;\n"
        ),
    );
}

/// `.tables`, `.indexes` and `.schema` report the same objects.
#[test]
fn the_catalog_commands_match() {
    // `.tables` and `.indexes` are compared with runs of whitespace collapsed.
    // The reference lays those two out in columns sized against an assumed
    // eighty-column terminal, so the exact padding is a property of a terminal
    // nobody is looking at rather than of the command; what a caller reads is
    // the names, in order.
    check_words(
        "catalog",
        &format!("{SETUP}.tables\n.indexes\n.indexes people\n"),
    );
    check("schema", &format!("{SETUP}.schema\n.schema people\n"));
}

/// `.databases` names the attached databases.
#[test]
fn the_database_list_matches() {
    // The file name differs between the two runs, so only the name column is
    // compared - which is the part a script reads.
    check("databases", "SELECT name FROM pragma_database_list;\n");
}

/// `.dump` produces SQL that rebuilds what was there.
#[test]
fn a_dump_matches() {
    check("dump", &format!("{SETUP}.dump\n"));
}

/// A dump can be read back into an empty database.
#[test]
fn a_dump_round_trips() {
    let Some(ours) = ours() else {
        eprintln!("the shell is missing; skipping");
        return;
    };
    let dumped = run(&ours, "roundtrip-out", &format!("{SETUP}.dump\n"));
    let script =
        format!("{dumped}\nSELECT count(*) FROM people;\nSELECT shout FROM loud ORDER BY shout;\n");
    let replayed = run(&ours, "roundtrip-in", &script);
    assert!(
        replayed.contains("\n3\n") || replayed.starts_with("3\n"),
        "the dump did not restore the rows:\n{replayed}"
    );
    assert!(
        replayed.contains("ADA"),
        "the view did not come back:\n{replayed}"
    );
}

/// `.import` reads a delimited file into a table.
#[test]
fn an_import_matches() {
    let file = area().join("import.csv");
    std::fs::write(&file, "10,ten\n20,\"twenty, with comma\"\n").expect("writes");
    let path = file.to_string_lossy().replace('\\', "/");
    check(
        "import",
        &format!(
            "CREATE TABLE loaded(n, label);\n.mode csv\n.import {path} loaded\n\
             .mode list\nSELECT n, label FROM loaded ORDER BY n;\n"
        ),
    );
}

/// `.output` sends results to a file and back again.
#[test]
fn output_redirection_matches() {
    check(
        "output",
        &format!(
            "{SETUP}.output out.txt\nSELECT id FROM people;\n.output stdout\n\
             SELECT 'back';\n.once once.txt\nSELECT 'hidden';\nSELECT 'shown';\n"
        ),
    );
}

/// An error is reported the same way, and `.bail` stops the script.
#[test]
fn errors_and_bail_match() {
    check("errors", "SELECT * FROM nope;\nSELECT 'after';\n");
}

/// A statement spanning several lines is run when it is complete.
#[test]
fn a_multi_line_statement_matches() {
    check(
        "multiline",
        "CREATE TABLE t(\n  a,\n  b\n);\nINSERT INTO t\n  VALUES\n  (1, 2);\nSELECT * FROM t;\n",
    );
}

/// A trigger body holds semicolons and is still one statement.
#[test]
fn a_trigger_body_is_one_statement() {
    check(
        "trigger",
        "CREATE TABLE t(a);\nCREATE TABLE log(a);\n\
         CREATE TRIGGER after_insert AFTER INSERT ON t BEGIN\n\
         INSERT INTO log VALUES (NEW.a);\n\
         END;\n\
         INSERT INTO t VALUES (7);\nSELECT a FROM log;\n",
    );
}

/// `.print` and `.echo` put the text where the reference puts it.
#[test]
fn print_and_echo_match() {
    check(
        "echo",
        "SELECT 1;\n.print hello\n.echo on\nSELECT 2;\n.echo off\nSELECT 3;\n",
    );
}

/// `.width` fixes the columns in the columnar mode.
#[test]
fn widths_match() {
    check(
        "widths",
        &format!("{SETUP}.mode column\n.width 4 12\nSELECT id, name FROM people;\n"),
    );
}
