//! The SQLLogicTest conformance suite.
//!
//! Invariant: the expected values in these files are SQLite's, recorded by
//! `inillucent-slt` from the pinned binary and checked in. Nothing in this file
//! computes an expectation, so a run grades inillucent against the reference
//! engine even when the oracle is not present on the machine.
//!
//! The format is SQLLogicTest's, so an upstream `.test` file dropped into
//! `tests/conformance/` runs here without a line of new code.

use std::path::PathBuf;

use inillucent_compat::facade::Database;
use inillucent_compat::slt::{self, Record, TestFile};
use inillucent_compat::workspace_root;
use inillucent_value::Value;

/// Returns every conformance file.
fn test_files() -> Vec<PathBuf> {
    let directory = workspace_root().join("tests/conformance");
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "test")
        })
        .collect();
    files.sort();
    files
}

/// Opens the corpus database the files were recorded against.
fn connect() -> inillucent_compat::facade::Connection {
    let path = workspace_root().join("compat/fixtures/select-corpus.db");
    let database = Database::import_staged(&path, "conformance").expect("the corpus fixture opens");
    database.session().expect("the connection opens")
}

/// Renders one row of values the way the format writes them.
fn render_row(values: &[Value<'static>], types: &str) -> Vec<String> {
    let letters: Vec<char> = types.chars().collect();
    values
        .iter()
        .enumerate()
        .map(|(index, value)| slt::render_value(value, letters.get(index).copied().unwrap_or('T')))
        .collect()
}

/// Every query in every conformance file returns exactly what SQLite returned.
#[test]
fn the_foundational_select_suite_is_green() {
    let files = test_files();
    assert!(!files.is_empty(), "no conformance files were found");
    let connection = connect();
    let mut checked = 0usize;
    let mut failures = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path).expect("the file reads");
        let file = TestFile::parse(&text).expect("the file parses");
        for record in &file.records {
            let Record::Query {
                types,
                sort,
                sql,
                expected,
                ..
            } = record
            else {
                // `statement ok` records describe how the fixture was built.
                // The fixture is checked in, so replaying them would be
                // rewriting a file SQLite wrote.
                continue;
            };
            checked = checked.saturating_add(1);
            let mut statement = match connection.prepare(sql) {
                Ok(statement) => statement,
                Err(failure) => {
                    failures.push(format!("{sql}: prepare failed: {failure}"));
                    continue;
                }
            };
            let mut rows = Vec::new();
            loop {
                match statement.step() {
                    Ok(true) => rows.push(render_row(statement.row(), types)),
                    Ok(false) => break,
                    Err(failure) => {
                        failures.push(format!("{sql}: step failed: {failure}"));
                        break;
                    }
                }
            }
            let found = slt::apply_sort(rows, sort);
            if &found != expected {
                failures.push(format!(
                    "{sql}\n  expected {expected:?}\n  found    {found:?}"
                ));
            }
        }
    }
    assert!(checked > 100, "only {checked} queries were checked");
    assert!(
        failures.is_empty(),
        "{} of {checked} queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The conformance files are readable as the format defines it, and round-trip
/// through the parser, so a hand-written file and a generated one are the same.
#[test]
fn the_conformance_files_round_trip() {
    for path in test_files() {
        let text = std::fs::read_to_string(&path).expect("the file reads");
        let file = TestFile::parse(&text).expect("the file parses");
        assert_eq!(
            file.render().replace("\r\n", "\n"),
            text.replace("\r\n", "\n"),
            "{}",
            path.display()
        );
    }
}
