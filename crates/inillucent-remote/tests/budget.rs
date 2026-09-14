//! What a migration is allowed to spend.
//!
//! Invariant: **a migration runs inside the request's budget, so the thing that
//! started it can stop it.** A migration is the longest thing this workspace
//! does on one call: it reads a whole database over a socket and writes it
//! through a second connection, and how long that takes is decided by somebody
//! else's server.
//!
//! ## The defect (task-1932, H11)
//!
//! `copy_table`'s scan called nothing that read the budget. A migration runs
//! outside the executor, so none of the engine's own checks are on its path:
//! the sixty second deadline an MCP server arms never fired, a cancellation had
//! nothing to land on, and `inillucent migrate` against a large table held the
//! server for hours with the client unable to do anything but kill it.
//!
//! The check is per batch rather than per row, which is what the case below
//! drives: a batch of one, so every row is a check, and a source slow enough
//! that the deadline is reached long before the rows run out.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use inillucent_base::budget::{self, Limits};
use inillucent_base::DbResult;
use inillucent_remote::migrate::{run, staging_path, Plan};
use inillucent_remote::source::{Kind, SourceColumn, SourceTable};
use inillucent_remote::{ConnectionUrl, RemoteSource};
use inillucent_tree::datum::OwnedDatum;

/// How long each row of the slow source takes to arrive.
///
/// A migration's cost is the server's, not this process's, so the stub spends
/// the time the way a real source does: waiting. Twenty milliseconds a row is
/// slow enough that the deadline below is reached in tens of rows rather than
/// thousands, which keeps the case under two seconds.
const PER_ROW: Duration = Duration::from_millis(20);

/// How many rows the slow source holds.
///
/// Far more than the deadline can reach, so a case that finished the copy would
/// be a case that never looked at the budget.
const ROWS: usize = 500;

/// The deadline the migration runs under.
const DEADLINE: Duration = Duration::from_millis(700);

/// A source whose rows arrive slowly, the way a remote server's do.
struct Slow {
    /// The one table it describes.
    table: SourceTable,
    /// How many rows it has handed over.
    sent: u64,
}

impl Slow {
    /// Returns a source of [`ROWS`] rows in one table.
    fn new() -> Slow {
        Slow {
            table: SourceTable {
                schema: "public".to_string(),
                name: "note".to_string(),
                target: "note".to_string(),
                columns: vec![
                    SourceColumn {
                        name: "id".to_string(),
                        declared: "bigint".to_string(),
                        kind: Kind::Integer,
                        nullable: false,
                    },
                    SourceColumn {
                        name: "body".to_string(),
                        declared: "text".to_string(),
                        kind: Kind::Text,
                        nullable: true,
                    },
                ],
                primary_key: vec!["id".to_string()],
            },
            sent: 0,
        }
    }
}

impl RemoteSource for Slow {
    fn describe(&mut self) -> DbResult<Vec<SourceTable>> {
        Ok(vec![self.table.clone()])
    }

    fn count(&mut self, _table: &SourceTable) -> DbResult<u64> {
        Ok(ROWS as u64)
    }

    fn scan(
        &mut self,
        _table: &SourceTable,
        sink: &mut dyn FnMut(&[OwnedDatum]) -> DbResult<()>,
    ) -> DbResult<u64> {
        for nth in 0..ROWS {
            std::thread::sleep(PER_ROW);
            let row = [
                OwnedDatum::Int(nth as i64),
                OwnedDatum::Text(format!("row {nth}").into_bytes()),
            ];
            sink(&row)?;
            self.sent = self.sent.saturating_add(1);
        }
        Ok(self.sent)
    }

    fn server(&self) -> String {
        "StubSQL 1.0".to_string()
    }

    fn not_carried(&mut self) -> DbResult<Vec<(String, String)>> {
        Ok(Vec::new())
    }

    fn finish(&mut self) {}

    fn peer(&self) -> Option<String> {
        Some("a stub that answers slowly".to_string())
    }
}

/// Where this suite's scratch databases live.
fn scratch(name: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../_agent_output/remote-budget")
        .to_path_buf();
    let _ = std::fs::create_dir_all(&directory);
    let destination = directory.join(format!("{name}.rdb"));
    let _ = std::fs::remove_file(&destination);
    let _ = std::fs::remove_file(staging_path(&destination));
    let _ = std::fs::remove_file(destination.with_file_name(format!("{name}.migration-report.md")));
    destination
}

/// Returns a plan against a URL that is never dialled.
///
/// @param destination - where to publish
/// @param limits - what the migration may spend
fn plan(destination: &Path, limits: Option<Limits>) -> Plan {
    let url = ConnectionUrl::parse("postgres://user:hunter2@example:5432/corpus").expect("parses");
    let mut plan = Plan::new(url, destination);
    // One row per destination transaction, so the budget is read once per row.
    // A migration in production uses a batch of thousands; this is the setting
    // that makes the *frequency* of the check observable rather than the check
    // itself, which is the same either way.
    plan.batch = 1;
    plan.write_report = false;
    plan.limits = limits;
    plan
}

/// A migration that runs past its deadline stops, and publishes nothing.
///
/// **It ran to the end before this (task-1932, H11).** The refusal has to name
/// the budget rather than any other failure, and the destination has to be
/// absent afterwards: a migration publishes by renaming, so a stopped one that
/// left a database behind would be worse than one that ran too long.
#[test]
fn a_migration_past_its_deadline_stops_and_publishes_nothing() {
    let destination = scratch("deadline");
    let started = Instant::now();
    let outcome = run(
        &plan(
            &destination,
            Some(Limits::unbounded().with_time(Some(DEADLINE))),
        ),
        &mut Slow::new(),
    );
    let elapsed = started.elapsed();

    let error = outcome.err().unwrap_or_else(|| {
        panic!(
            "a migration of {ROWS} rows at {PER_ROW:?} a row finished inside a {DEADLINE:?} \
             deadline, which means nothing on its path read the budget"
        )
    });
    let said = error
        .detail()
        .map(str::to_string)
        .unwrap_or_else(|| error.message().to_string());
    assert!(
        said.contains("time it was allowed"),
        "the migration stopped for something other than its deadline: {said}"
    );
    assert!(
        elapsed < PER_ROW.saturating_mul(ROWS as u32 / 4),
        "the migration took {elapsed:?}, which is most of the way through {ROWS} rows: it \
         stopped at the end rather than at its deadline"
    );
    assert!(
        !destination.exists(),
        "a migration that was stopped published {} anyway",
        destination.display()
    );
}

/// The same migration finishes when its budget allows it to.
///
/// The other half of the pair. A refusal is evidence that the budget stops a
/// migration only if the same migration completes without one - otherwise a
/// stub that simply fails reads exactly the same.
#[test]
fn the_same_migration_finishes_with_no_deadline() {
    let destination = scratch("unbounded");
    let outcome = run(
        &plan(&destination, Some(Limits::unbounded())),
        &mut Slow::new(),
    );
    let report = outcome.unwrap_or_else(|error| {
        panic!(
            "the migration failed with no deadline: {}",
            error
                .detail()
                .map(str::to_string)
                .unwrap_or_else(|| error.message().to_string())
        )
    });
    assert_eq!(
        report.tables.first().map(|table| table.rows),
        Some(ROWS as u64),
        "the migration published a different number of rows than the source held"
    );
    assert!(
        destination.exists(),
        "a migration that reported success published nothing"
    );
}

/// A migration under a cancellation flag stops when the flag is set.
///
/// **The deadline and the cancel are the same check and different journeys.**
/// A deadline is decided when the request starts; a cancel arrives from
/// somewhere else while it runs, which is what an MCP client sends and what the
/// C ABI's `inillucent_cancel` sets. Both reach `copy_table` through
/// `budget::check`, and only one of them can be asserted by waiting.
#[test]
fn a_migration_stops_when_its_request_is_cancelled() {
    let destination = scratch("cancelled");
    let flag = Arc::new(AtomicBool::new(false));
    // Armed here rather than through the plan, because the flag has to be one
    // this test still holds a handle to.
    let armed = budget::arm(Limits::unbounded(), Arc::clone(&flag));

    let setter = Arc::clone(&flag);
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(DEADLINE);
        setter.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    let started = Instant::now();
    let outcome = run(&plan(&destination, None), &mut Slow::new());
    let elapsed = started.elapsed();
    let _ = stopper.join();
    drop(armed);

    let error = outcome
        .err()
        .unwrap_or_else(|| panic!("a cancelled migration finished after {elapsed:?}"));
    let said = error
        .detail()
        .map(str::to_string)
        .unwrap_or_else(|| error.message().to_string());
    assert!(
        said.contains("cancelled"),
        "the migration stopped for something other than the cancellation: {said}"
    );
    assert!(
        !destination.exists(),
        "a cancelled migration published {} anyway",
        destination.display()
    );
}
