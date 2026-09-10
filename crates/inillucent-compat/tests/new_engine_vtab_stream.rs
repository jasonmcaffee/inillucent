//! A scan over a virtual table can be stopped, and a hang is a test with a
//! deadline.
//!
//! Invariant: **an operator above a virtual-table scan can stop it.** A `LIMIT`
//! that cannot stop the scan below it is not a slow query, it is one that does
//! not return: `generate_series` with no `stop` constraint is 4,294,967,295
//! rows, and `TreeCatalog::virtual_rows` materialised the whole scan into a
//! `Vec` before any operator above it ran. Three shapes ran past a 25-second
//! timeout, one run holding about 1.2 cores and a growing working set for ten
//! minutes.
//!
//! ## Why every case here carries a deadline
//!
//! Because the failure mode is a hang, and an assertion on rows is never
//! reached by a statement that never returns - so a test written the ordinary
//! way would hang the whole suite rather than fail it. Each case runs its
//! statement on its own thread and waits on a channel with a timeout; the
//! deadline expiring *is* the failure. The thread is left behind when that
//! happens, which is correct: the point is that the test process reports rather
//! than waits, and a test run that has already failed is on its way out.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;

/// How long a bounded statement may take before the case is a failure.
///
/// Generous by three orders of magnitude against the answer being measured -
/// these all run in well under a millisecond - because the number exists to
/// separate "returned" from "did not return", not to grade a duration.
const DEADLINE: Duration = Duration::from_secs(10);

/// Runs one statement against a fresh database, with a deadline.
///
/// The database is created inside the worker thread, because the engine's
/// handles are not `Send`: what crosses the channel is the answer, rendered.
///
/// @param tag - what to name the database
/// @param setup - statements to run first
/// @param sql - the statement whose rows are the answer
fn answer_within(tag: &str, setup: &[&str], sql: &str) -> (Vec<String>, Duration) {
    let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("vtab-stream")
        .join(format!("{tag}.rdb"));
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
    let _ = std::fs::remove_file(&path);
    let (sender, receiver) = mpsc::channel();
    let owned: Vec<String> = setup.iter().map(|held| (*held).to_string()).collect();
    let statement = sql.to_string();
    let started = Instant::now();
    std::thread::spawn(move || {
        let answer = (|| -> Result<Vec<String>, String> {
            let mut database = ImportedDatabase::create(path, 4_096, 64)
                .map_err(|error| error.message().to_string())?;
            for held in &owned {
                database
                    .execute_any(held, &Params::new())
                    .map_err(|error| format!("{held}: {}", error.message()))?;
            }
            let outcome = database
                .execute_any(&statement, &Params::new())
                .map_err(|error| format!("{statement}: {}", error.message()))?;
            Ok(outcome
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|value| format!("{value:?}"))
                        .collect::<Vec<String>>()
                        .join("|")
                })
                .collect())
        })();
        let _ = sender.send(answer);
    });
    match receiver.recv_timeout(DEADLINE) {
        Ok(Ok(rows)) => (rows, started.elapsed()),
        Ok(Err(reason)) => panic!("{tag}: {reason}"),
        Err(_) => panic!(
            "{tag}: `{sql}` did not return within {DEADLINE:?} - the scan below the LIMIT \
             cannot be stopped"
        ),
    }
}

/// A `LIMIT` stops an unbounded series.
///
/// The reason Part C exists. `generate_series(1)`
/// with no `stop` is every integer up to `u32::MAX`; three rows of it have to
/// come back at once.
///
/// **The deadline is the whole assertion, and there used to be a second one.**
/// This also asserted that the three rows arrived inside a second, which is a
/// grade rather than a test: the defect it guards against does not return at
/// all, so a second reading cannot separate a pass from a failure that the
/// deadline has not already separated - and a wall-clock grade on a machine
/// running two dozen other test binaries is decided by the machine. The
/// sibling case below crossed a 500 ms bound of exactly this kind during
/// task-1886's verification, at 636 ms, on an engine that was working.
#[test]
fn a_limit_stops_an_unbounded_series() {
    let (rows, elapsed) = answer_within(
        "unbounded-limit",
        &[],
        "SELECT value FROM generate_series(1) LIMIT 3",
    );
    assert_eq!(rows.len(), 3, "took {elapsed:?}, rows {rows:?}");
}

/// The same, through `CREATE VIRTUAL TABLE`.
///
/// This is the shape that could be reached *before* the eponymous form
/// existed - so it is the case that isolates the
/// materialisation defect from the binding one. On the tree before this change
/// it runs past the deadline; the eponymous cases above merely report
/// `no such table`.
#[test]
fn a_limit_stops_a_created_series() {
    let (rows, elapsed) = answer_within(
        "created-limit",
        &["CREATE VIRTUAL TABLE gs USING generate_series"],
        "SELECT value FROM gs LIMIT 3",
    );
    assert_eq!(rows.len(), 3, "took {elapsed:?}, rows {rows:?}");
}

/// A `LIMIT` stops a series whose length the module already knows.
///
/// H3's own case is `SELECT value FROM generate_series(1,10) LIMIT 3`, and it
/// is the first arm here. **On its own it cannot fail**: ten rows are cheap to
/// materialise, so a scan that ignores the `LIMIT` entirely still answers it,
/// and the guard that stood in for the missing evidence was a stopwatch - "the
/// whole run took 636.3788ms" against a 500 ms bound, which is what it printed
/// under task-1886's load while the engine was working.
///
/// So the second arm asks the same question of a series with four billion rows
/// in it. The `stop` is given, so nothing about it is open-ended; the only way
/// three rows come back inside the deadline is that the `LIMIT` stopped the
/// scan below it, and the materialising scan this file exists to catch cannot
/// finish it in any amount of time a test will wait. That is a test with two
/// outcomes rather than a duration with a grade, and its bound is the file's
/// [`DEADLINE`] - three orders of magnitude away from the answer, as the header
/// says a number here should be.
#[test]
fn a_limit_stops_a_series_whose_length_is_known() {
    let (rows, elapsed) = answer_within(
        "bounded-limit",
        &[],
        "SELECT value FROM generate_series(1,10) LIMIT 3",
    );
    assert_eq!(rows.len(), 3, "took {elapsed:?}, rows {rows:?}");

    let (long, elapsed) = answer_within(
        "bounded-limit-long",
        &[],
        "SELECT value FROM generate_series(1,4000000000) LIMIT 3",
    );
    assert_eq!(
        long, rows,
        "four billion rows and ten gave different answers to the same LIMIT 3 \
         ({elapsed:?})"
    );
}
/// The eponymous form exists at all, for each of the three shapes.
///
/// `FROM generate_series(1,10)`, `FROM json_each(...)` and
/// `FROM pragma_table_info('t')` were all `no such table` before the eponymous
/// forms were added, which also left `json_each` unreachable from SQL by any
/// route - its module refuses `CREATE VIRTUAL TABLE` outright.
#[test]
fn the_eponymous_forms_bind() {
    let (series, _) = answer_within(
        "eponymous-series",
        &[],
        "SELECT count(*), sum(value) FROM generate_series(1,10)",
    );
    assert_eq!(series, vec!["Int(10)|Int(55)".to_string()]);

    let (each, _) = answer_within(
        "eponymous-json",
        &[],
        "SELECT count(*) FROM json_each('[10,20,30]')",
    );
    assert_eq!(each, vec!["Int(3)".to_string()]);

    let (info, _) = answer_within(
        "eponymous-pragma",
        &["CREATE TABLE t(a INTEGER, b TEXT)"],
        "SELECT count(*) FROM pragma_table_info('t')",
    );
    assert_eq!(info, vec!["Int(2)".to_string()]);
}

/// A module's constraints still reach it through the eponymous form.
///
/// The arguments a caller writes are bound as `Eq` constraints on the module's
/// hidden columns, which is what `best_index` is already written to consume -
/// and is why the series' `stop` was missing before the form existed.
#[test]
fn the_arguments_reach_the_module() {
    let (stepped, _) = answer_within(
        "eponymous-step",
        &[],
        "SELECT group_concat(value, ',') FROM generate_series(0,10,5)",
    );
    assert_eq!(stepped, vec!["Text([48, 44, 53, 44, 49, 48])".to_string()]);
}
