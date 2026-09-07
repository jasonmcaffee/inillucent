//! The capability table, checked against the engine in both directions.
//!
//! This is the driver's most important test and the reason the table is worth
//! having at all. `java.sql.DatabaseMetaData` has had `supportsFullOuterJoins()`
//! since 1997 and its answers are famously unreliable, because every driver
//! hand-writes them and nothing runs them. A capability list nobody checks is a
//! list of claims that were true once.
//!
//! So every row is run:
//!
//! - declared **supported** and the probe fails → the driver was lying about
//!   something it offered.
//! - declared **unsupported** and the probe *succeeds* → the engine has grown
//!   it, and the table is stale. This half is what stops the list decaying while
//!   Phase 6 closes the gaps it names.
//!
//! A row with no probe is exempt and has to say why in its note, which
//! `capability.rs`'s own unit test enforces.

use std::path::PathBuf;

use inillucent_driver::capability::{Probe, Support, CAPABILITIES};
use inillucent_driver::{Database, Status, Value};

/// Returns a scratch database nothing else is using.
///
/// One file per probe, because a probe leaves a schema behind and the next
/// probe's `CREATE TABLE` would meet it. A shared file would also make the
/// order of the table significant, which is exactly the kind of coupling that
/// makes a failing row hard to read.
///
/// @param name - what to call the file
fn scratch(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "inillucent-driver-cap-{name}-{}-{:?}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0)
    ));
    path
}

/// What running one probe produced.
enum Outcome {
    /// It ran.
    Ran,
    /// It was refused, with the driver's word for why.
    Refused(String),
    /// It answered, and this is its first cell rendered.
    Answered(String),
}

/// Runs one capability's setup and probe against a fresh database.
///
/// @param name - the capability's name, used for the file
/// @param setup - the fixture statements
/// @param sql - the statement under test
/// @param want_value - whether the probe reads a value rather than an outcome
fn probe(name: &str, setup: &[&str], sql: &str, want_value: bool) -> Outcome {
    let path = scratch(name);
    let database = Database::open(&path).expect("the scratch database opens");
    let connection = database.connect();
    for statement in setup {
        // A setup statement that is refused is a fact about the engine too, and
        // silently carrying on would make the probe measure something else. The
        // one exception is `PRAGMA foreign_keys`, which the engine answers and
        // does not honour - that is the very thing the row is testing.
        if let Err(why) = connection.query(statement, &[], usize::MAX) {
            if !statement.starts_with("PRAGMA") {
                panic!("{name}: the fixture statement `{statement}` was refused: {why}");
            }
        }
    }
    let outcome = match connection.query(sql, &[], usize::MAX) {
        Err(why) => Outcome::Refused(format!("{}: {}", why.status.name(), why.message)),
        Ok(rows) if want_value => Outcome::Answered(match rows.value(0, 0) {
            Some(Value::Text(text)) => text.clone(),
            Some(Value::Integer(number)) => number.to_string(),
            Some(Value::Real(number)) => number.to_string(),
            Some(Value::Blob(_)) => "<bytes>".to_owned(),
            Some(Value::Null) | None => "<null>".to_owned(),
        }),
        Ok(_) => Outcome::Ran,
    };
    drop(connection);
    drop(database);
    let _ = std::fs::remove_file(&path);
    outcome
}

/// Every declared capability is what the engine actually does, in both
/// directions.
#[test]
fn the_capability_table_matches_the_engine() {
    let mut wrong: Vec<String> = Vec::new();
    for entry in CAPABILITIES {
        let supported = entry.support == Support::Yes;
        match entry.probe {
            Probe::Nothing => continue,
            Probe::Runs { setup, sql } => match (supported, probe(entry.name, setup, sql, false)) {
                (true, Outcome::Ran) | (false, Outcome::Refused(_)) => {}
                (true, Outcome::Refused(why)) => wrong.push(format!(
                    "`{}` is declared supported and was refused - {why}",
                    entry.name
                )),
                (false, Outcome::Ran) => wrong.push(format!(
                    "`{}` is declared unsupported and it WORKED. The engine has grown it; \
                     update capability.rs rather than this test.",
                    entry.name
                )),
                (_, Outcome::Answered(_)) => unreachable!("Runs never reads a value"),
            },
            Probe::Refuses { setup, sql } => {
                match (supported, probe(entry.name, setup, sql, false)) {
                    (true, Outcome::Refused(_)) | (false, Outcome::Ran) => {}
                    (true, Outcome::Ran) => wrong.push(format!(
                        "`{}` is declared supported, and the statement that should have been \
                         refused succeeded",
                        entry.name
                    )),
                    (false, Outcome::Refused(why)) => wrong.push(format!(
                        "`{}` is declared unsupported and the engine REFUSED the violating \
                         statement - {why}. The engine has grown it; update capability.rs.",
                        entry.name
                    )),
                    (_, Outcome::Answered(_)) => unreachable!("Refuses never reads a value"),
                }
            }
            Probe::Answers { setup, sql, expect } => {
                match (supported, probe(entry.name, setup, sql, true)) {
                    (true, Outcome::Answered(got)) if got == expect => {}
                    (false, Outcome::Answered(got)) if got != expect => {}
                    (true, other) => wrong.push(format!(
                        "`{}` is declared supported and answered `{}` where `{expect}` was \
                         required",
                        entry.name,
                        described(&other)
                    )),
                    (false, other) => wrong.push(format!(
                        "`{}` is declared unsupported and answered `{expect}` anyway ({}). The \
                         engine has grown it; update capability.rs.",
                        entry.name,
                        described(&other)
                    )),
                }
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "the capability table does not describe this engine:\n  - {}",
        wrong.join("\n  - ")
    );
}

/// Renders an outcome for a failure message.
///
/// @param outcome - what the probe produced
fn described(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Ran => "it ran and returned no value".to_owned(),
        Outcome::Refused(why) => format!("refused: {why}"),
        Outcome::Answered(value) => value.clone(),
    }
}

/// A construct the engine has not implemented is refused as
/// [`Status::Unsupported`] and names itself, rather than arriving as the syntax
/// error a typo gets.
///
/// This is the distinction the whole driver is built around, and it is asserted
/// here against the real engine rather than against a constructed error.
#[test]
fn an_unimplemented_construct_refuses_by_name_and_a_typo_does_not() {
    let path = scratch("classify");
    let database = Database::open(&path).expect("opens");
    let connection = database.connect();
    connection
        .query("CREATE TABLE people (a INTEGER PRIMARY KEY)", &[], 0)
        .expect("the fixture is made");
    connection
        .query("CREATE TABLE teams (a INTEGER PRIMARY KEY)", &[], 0)
        .expect("the fixture is made");

    // **The example moves as the engine grows, and that is the point.** It was
    // a `LEFT JOIN` until task-1838 implemented that, and `ATTACH` until
    // task-1844 implemented that; the assertion is about the *classification*,
    // so it is repointed at a construct that is still unimplemented rather than
    // weakened. `VACUUM` is one, and the capability table says so in the row
    // beside this one.
    let refused = connection
        .query("VACUUM", &[], 10)
        .expect_err("VACUUM is refused");
    assert_eq!(
        refused.status,
        Status::Unsupported,
        "VACUUM is a capability gap, not a syntax error: {refused}"
    );
    let named = refused.feature.expect("the refusal names the construct");
    assert!(named.contains("VACUUM"), "it named `{named}`");

    let typo = connection
        .query("SELECT a FROM peple", &[], 10)
        .expect_err("a missing table is refused");
    assert_eq!(
        typo.status,
        Status::NotFound,
        "a mistyped table name is not a capability gap: {typo}"
    );
    assert_eq!(typo.feature, None, "and it names no construct");

    let malformed = connection
        .query("SELECT a FORM people", &[], 10)
        .expect_err("a malformed statement is refused");
    assert_eq!(malformed.status, Status::Syntax, "{malformed}");
    assert_eq!(malformed.feature, None);

    drop(connection);
    drop(database);
    let _ = std::fs::remove_file(&path);
}
