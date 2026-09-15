//! One scenario, asked of both engines, compared in full.
//!
//! Invariant: nothing here decides what "the same" means for a particular
//! feature. It runs a script against a live SQLite process and against
//! inillucent, and compares everything the protocol carries - the rows and their
//! storage classes, the column names, the change counters, the autocommit flag,
//! and, when a statement fails, its primary and extended result codes. A test
//! that wanted a looser comparison would be testing something else.
//!
//! This lived twice over in `dml_differential.rs` and `foreign_keys.rs` before
//! phase 11 needed it six more times. Copying it again would have meant six
//! chances for one copy to compare less than the others and look green.

// **This module may panic, and the crate-level deny does not reach it.** It is
// the harness that starts the pinned SQLite oracle and this engine beside it,
// and it lives in `src/` so that every `tests/*.rs` target can use it - which
// means it is compiled without `cfg(test)` and the crate's test-only relaxation
// does not apply. An oracle that will not open is a broken environment rather
// than a result, and a suite that carried on would be comparing this engine
// against nothing.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::PathBuf;

use crate::oracle::{Driver, Observation, Op, TaggedValue};
use crate::workspace_root;
use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

/// One step of a scenario.
#[derive(Clone, Copy, Debug)]
pub enum Step {
    /// SQL that is not expected to return rows.
    Exec(&'static str),
    /// SQL whose rows are compared.
    Query(&'static str),
}

/// Returns the pinned SQLite oracle binary, if it has been built.
pub fn sqlite_oracle() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    // The name is chosen by this platform's executable suffix rather than by
    // trying both, because both exist: the workspace is shared between Windows
    // and WSL, and a Linux run that picked up `sqlite-oracle.exe` would start
    // it through the interop layer and then hand a Windows process a `/mnt/c`
    // path it cannot open.
    let directory = workspace_root().join(".sqlite-ref/3.53.4");
    let path = directory.join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Announces a skipped run, in the words the rest of the harness uses.
pub fn announce_skip() {
    skipping("the pinned SQLite oracle is not built; run tools/sqlite-reference.{ps1,sh}");
}

/// Says why a case did not run, and fails the case when the run is strict.
///
/// **One marker and one decision, in one place (task-1932, H10).** Every skip
/// site in the workspace ends its message with `; skipping`, which is what
/// `testrun`'s classifier matches and what `tests/inillucent-testing-tdd.md`
/// §9 asks for. Before this there were three phrasings and a list of six
/// substrings trying to catch them, and two of the phrasings - the TLS
/// transport suite's `case skipped` and the ONNX suites' `skipping:` prefix -
/// matched none of them. The TLS suite in particular runs other tests in the
/// same binary, so it was invisible to `--strict` by both routes: a CI image
/// without Python's `ssl` module passed the TLS verification suite without
/// running it.
///
/// **`INILLUCENT_STRICT` makes the skip a failure of the test rather than a
/// classification of the binary.** `inillucent-testrun --strict` sets it, and
/// the panic then names the test and the thing that is missing. The classifier
/// stays as the backstop for a suite that skips some other way; this is the
/// same decision made one layer earlier, where there is still a test on the
/// stack to name.
///
/// @param reason - what is missing, without the marker
pub fn skipping(reason: &str) {
    if std::env::var("INILLUCENT_STRICT").is_ok_and(|value| !value.is_empty()) {
        panic!("{reason}; skipping{STRICT_SKIP}");
    }
    eprintln!("{reason}; skipping");
}

/// What a strict run's skip panic says after the marker.
///
/// **A skip and a failure are different things and the report has to keep them
/// apart (task-1932, H10).** `--strict` makes a skip fail the test, which is
/// what names the case rather than the binary - but a suite that skipped did
/// not evidence a problem, it evidenced nothing, and listing it under FAILED
/// would put a second wrong label on the same event. `missing_prerequisites`
/// reads this sentinel to tell one from the other: a target whose every failure
/// carries it is hollow, and a target with even one failure that does not is a
/// failure.
pub const STRICT_SKIP: &str = " - and this run is strict, so a skip is a failure";

/// Reports whether every failure in a transcript is a strict skip.
///
/// False when nothing failed, so a caller cannot read "no failures" as "every
/// failure was a skip".
///
/// @param output - everything the suite printed
/// @param failed - how many of its tests failed
pub fn every_failure_is_a_strict_skip(output: &str, failed: usize) -> bool {
    failed > 0 && output.matches(STRICT_SKIP).count() >= failed
}

/// Reports whether a transcript says a suite skipped.
///
/// **One phrase, and it is the phrase every site prints.** This used to be six
/// substrings - `has not been built`, `is not built`, `is not available`,
/// `is missing`, `no reference` and `; skipping` - and the suites it was
/// written for printed three phrasings, two of which matched none of the six.
/// A list of near misses is a list nobody checks against, so the list is gone
/// and `policy::every_skip_site_carries_the_one_marker` greps for the marker
/// instead: a message written the old way now fails that check rather than
/// being quietly half-recognised here (task-1932, H10).
///
/// @param output - everything the suite printed
pub fn announces_a_skip(output: &str) -> bool {
    output.contains("; skipping")
}

/// Returns a fresh path for one engine's copy of one scenario.
pub fn scratch(area: &str, name: &str, engine: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output").join(area);
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join(format!("{name}-{engine}.db"));
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(directory.join(format!("{name}-{engine}.db{suffix}")));
    }
    path
}

/// Starts the oracle on a fresh database, or reports why it could not.
pub fn start_oracle(area: &str, name: &str) -> Option<Driver> {
    let program = sqlite_oracle()?;
    let path = scratch(area, name, "sqlite");
    let mut driver = Driver::start("sqlite", &program).ok()?;
    let hello = driver.send(&Op::Hello).ok()?;
    assert!(hello.ok, "the oracle did not answer hello");
    let opened = driver
        .send(&Op::Open(path.display().to_string()))
        .expect("the oracle opens its database");
    assert!(opened.ok, "the oracle could not open {}", path.display());
    Some(driver)
}

/// Opens inillucent on its own copy of a scenario's database.
///
/// **Leaked rather than borrowed.** The new engine's `Connection<'d>` borrows
/// the `Database` it came from - the pool is the database's, and a connection
/// is a session number against it - so a helper that hands back a bare
/// `Connection` the way the old one did needs the database to outlive the
/// function that opened it. A scenario's database is opened once and used for
/// exactly one test, so leaking it for the process's lifetime costs nothing a
/// test run cares about and keeps every call site that used to write
/// `start_inillucent(area, name)` unchanged.
pub fn start_inillucent(area: &str, name: &str) -> Connection<'static> {
    let path = scratch(area, name, "inillucent");
    let database: &'static Database = Box::leak(Box::new(
        Database::open(&path).expect("inillucent opens its database"),
    ));
    let connection = database.session();
    // Matches the old engine's default: the harness compares single-connection
    // scenarios against a separate oracle process, so nothing here contends for
    // the lock, but a scenario that does open a second connection should not
    // fail on a busy database for want of a timeout the old engine always set.
    let _ = connection.execute_batch("PRAGMA busy_timeout = 5000");
    connection
}

/// Renders a inillucent value as the tagged value the protocol carries.
pub fn tagged(value: &OwnedDatum) -> TaggedValue {
    match value {
        OwnedDatum::Null => TaggedValue::Null,
        OwnedDatum::Int(integer) => TaggedValue::Integer(*integer),
        OwnedDatum::Real(real) => TaggedValue::Real(*real),
        OwnedDatum::Text(text) => TaggedValue::Text(text.clone()),
        OwnedDatum::Blob(blob) => TaggedValue::Blob(blob.clone()),
    }
}

/// Runs one statement on inillucent and reports it the way the oracle would.
///
/// The shapes have to match exactly, including the parts that are easy to get
/// almost right: a failed statement still reports the connection state after
/// it, and a query that produced no rows still reports its column names.
pub fn observe(connection: &Connection<'_>, sql: &str, query: bool) -> Observation {
    let mut observation = Observation::default();
    let outcome = (|| -> Result<(Vec<Vec<TaggedValue>>, Vec<String>), inillucent_base::DbError> {
        let mut rows = Vec::new();
        let mut columns = Vec::new();
        let mut offset = 0usize;
        while offset < sql.len() {
            let rest = sql.get(offset..).unwrap_or("");
            let prepared = connection.prepare_with_tail(rest)?;
            let (mut statement, consumed) = (prepared.statement, prepared.consumed);
            // **Read after the first step, not before it.** This engine
            // resolves a statement's result columns when it runs rather than
            // when it is prepared - `Statement::columns` says so in as many
            // words - so asking before the first `step` answers an empty list.
            // It was asked before, because the engine this suite used to drive
            // resolved them at prepare time, and the whole file therefore
            // compared an empty column list against SQLite's real one and
            // failed every case on a difference that was the harness's own.
            // Taken on every step rather than only the first so that a
            // statement returning no rows at all still reports its shape.
            while statement.step()? {
                if query {
                    if columns.is_empty() {
                        columns = statement.columns().to_vec();
                    }
                    rows.push(statement.row().iter().map(tagged).collect());
                }
            }
            if query && columns.is_empty() {
                columns = statement.columns().to_vec();
            }
            drop(statement);
            if consumed == 0 {
                break;
            }
            offset = offset.saturating_add(consumed);
        }
        Ok((rows, columns))
    })();
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
    observation.changes = connection.changes().unwrap_or_default();
    observation.total_changes = connection.total_changes().unwrap_or_default();
    observation.last_insert_rowid = connection.last_insert_rowid().unwrap_or_default();
    observation.autocommit = connection.autocommit().unwrap_or_default();
    observation
}

/// Compares one observation, saying whether the cumulative counters count.
pub fn compare_with_counters(
    label: &str,
    sql: &str,
    candidate: &Observation,
    reference: &Observation,
    query: bool,
    counters: bool,
) {
    assert_eq!(
        candidate.ok,
        reference.ok,
        "{label} `{sql}`: inillucent {} and SQLite {}\n  inillucent: {}\n  SQLite:  {}",
        if candidate.ok { "succeeded" } else { "failed" },
        if reference.ok { "succeeded" } else { "failed" },
        candidate.message,
        reference.message
    );
    if !reference.ok {
        assert_eq!(
            candidate.code, reference.code,
            "{label} `{sql}`: primary code\n  inillucent: {} ({})\n  SQLite:  {} ({})",
            candidate.code, candidate.message, reference.code, reference.message
        );
        assert_eq!(
            candidate.extended, reference.extended,
            "{label} `{sql}`: extended code\n  inillucent: {} ({})\n  SQLite:  {} ({})",
            candidate.extended, candidate.message, reference.extended, reference.message
        );
        // **The rows are not compared and the counters are**, which is why this
        // returns here rather than skipping the whole tail. A failed statement
        // produced no rows to compare, but it has counters and they are a
        // question with a right answer: `UPDATE OR FAIL` keeps the rows it
        // wrote, so `changes()` moves and `total_changes()` moves with it,
        // while an `ABORT` puts them back and neither moves. Returning early
        // used to skip that grading entirely, which is how the pair could read
        // `0 | 0` for every failed statement with no differential case ever
        // catching it.
        compare_counters(label, sql, candidate, reference, counters);
        return;
    }
    if query {
        assert_eq!(
            candidate.rows, reference.rows,
            "{label} `{sql}`: rows\n  inillucent: {:?}\n  SQLite:  {:?}",
            candidate.rows, reference.rows
        );
        assert_eq!(
            candidate.columns, reference.columns,
            "{label} `{sql}`: column names",
        );
    }
    compare_counters(label, sql, candidate, reference, counters);
}

/// Compares the three counters and the autocommit flag.
///
/// Split out so a statement that **failed** is graded on them too: it has no
/// rows to compare and every other assertion above is about rows, but the
/// counters after a failure are a question with a right answer - and one this
/// engine got wrong in both directions, reading `0 | 0` for every failed
/// statement regardless of whether `UPDATE OR FAIL` should have kept the rows
/// it wrote or `ABORT` should have put them back.
///
/// @param label - what to call this comparison in a failure
/// @param sql - the statement, for the message
/// @param candidate - this engine's observation
/// @param reference - the oracle's
/// @param counters - whether the cumulative counters are still comparable,
///   which a module in play makes false
fn compare_counters(
    label: &str,
    sql: &str,
    candidate: &Observation,
    reference: &Observation,
    counters: bool,
) {
    // A `CREATE VIRTUAL TABLE` leaves the counters holding whatever the module
    // did to its own shadow tables while it was being made, which is a fact
    // about the module rather than about the caller's rows.
    let module_create = sql
        .trim_start()
        .get(..21)
        .is_some_and(|head| head.eq_ignore_ascii_case("CREATE VIRTUAL TABLE "));
    if !module_create {
        assert_eq!(
            candidate.changes, reference.changes,
            "{label} `{sql}`: changes()",
        );
    }
    if counters && !module_create {
        assert_eq!(
            candidate.total_changes, reference.total_changes,
            "{label} `{sql}`: total_changes()",
        );
        assert_eq!(
            candidate.last_insert_rowid, reference.last_insert_rowid,
            "{label} `{sql}`: last_insert_rowid()",
        );
    }
    assert_eq!(
        candidate.autocommit, reference.autocommit,
        "{label} `{sql}`: autocommit",
    );
}

/// Runs a scenario against both engines and compares every reply.
///
/// Returns how many statements were compared, so a scenario that stopped early
/// cannot look like one that passed. Zero means the oracle was not built.
pub fn compare(area: &str, name: &str, steps: &[Step]) -> usize {
    let Some(mut oracle) = start_oracle(area, name) else {
        announce_skip();
        return 0;
    };
    let connection = start_inillucent(area, name);
    let mut compared = 0usize;
    // The cumulative counters stop being comparable the moment a module is in
    // play, because they then count the module's own statements as well.
    let mut counters = true;
    for (index, step) in steps.iter().enumerate() {
        let (sql, query) = match step {
            Step::Exec(sql) => (*sql, false),
            Step::Query(sql) => (*sql, true),
        };
        if sql
            .trim_start()
            .get(..21)
            .is_some_and(|head| head.eq_ignore_ascii_case("CREATE VIRTUAL TABLE "))
        {
            counters = false;
        }
        let op = if query {
            Op::Query(sql.to_string())
        } else {
            Op::Exec(sql.to_string())
        };
        let reference = oracle.send(&op).expect("the oracle answers");
        let candidate = observe(&connection, sql, query);
        compare_with_counters(
            &format!("step {index}"),
            sql,
            &candidate,
            &reference,
            query,
            counters,
        );
        compared = compared.saturating_add(1);
    }
    let _ = oracle.send(&Op::Bye);
    compared
}

/// Runs one query against both engines and compares the answer.
///
/// The convenience form for a suite whose cases are independent one-line
/// questions - which is what a built-in function's parity looks like.
pub fn compare_queries(area: &str, name: &str, queries: &[&'static str]) -> usize {
    let steps: Vec<Step> = queries.iter().map(|sql| Step::Query(sql)).collect();
    compare(area, name, &steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The literal messages the suites print are classified as skips, and the
    /// phrasings they used to print are not.
    ///
    /// **The second half is the point.** The two that were invisible - the TLS
    /// transport suite's `case skipped` and the ONNX suites' `skipping:` prefix
    /// - are asserted *unrecognised* here, because recognising them would make
    /// this classifier the thing that keeps them working and leave the marker
    /// optional. The marker is not optional; `policy.rs` greps for it. What
    /// this asserts is that the classifier and the grep agree on one rule.
    #[test]
    fn the_messages_the_suites_print_are_classified_as_skips() {
        for transcript in [
            "the pinned SQLite oracle is not built; run tools/sqlite-reference.{ps1,sh}; skipping",
            "transport: no python with an ssl module; skipping",
            "transport: no openssl to generate certificates; skipping",
            "transport: the fake server did not start; skipping",
            "no ONNX weights found; skipping",
            "could not load the model (out of memory); skipping",
            "no models root at /models; skipping",
            "the inillucent binary is not built; skipping",
            "budgets: the MCP server did not start; skipping",
            "running 4 tests
test a ... ok
the medium fixture is not built; skipping
",
        ] {
            assert!(
                announces_a_skip(transcript),
                "this is a skip and was not classified as one: {transcript}"
            );
        }
        // The phrasings that were invisible before task-1932, which are gone
        // from every site and must stay unrecognised here.
        for transcript in [
            "transport: no python with an ssl module; case skipped",
            "skipping: no ONNX weights found",
            "the reference has not been built",
            "the corpus is missing",
            "no reference available",
        ] {
            assert!(
                !announces_a_skip(transcript),
                "an old phrasing was recognised, which makes the marker optional: {transcript}"
            );
        }
        // And an ordinary transcript is not a skip.
        for transcript in [
            "running 12 tests
test result: ok. 12 passed; 0 failed",
            "warning: the corpus is large",
            "",
        ] {
            assert!(!announces_a_skip(transcript), "{transcript}");
        }
    }
}
