//! Power loss during a WAL commit, a checkpoint, and a recovery.
//!
//! Invariant: after any modelled failure the database is exactly what it was
//! before the transaction or exactly what it would have been after it, and
//! never a mixture - and a transaction that reported success is always the
//! second. A log makes that easier to promise than a journal does and no
//! easier to *test*, so it is tested the same way: every injectable call of the
//! run is numbered, and the run is repeated once per number with the failure
//! armed at exactly that call.
//!
//! The shared-memory index deliberately does not survive the crash. It is a
//! cache of the log and nothing else, so every one of these recoveries rebuilds
//! it by reading the log from its first byte - which is the path a real crash
//! takes and the one worth exercising.

use std::path::PathBuf;
use std::sync::Arc;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_sim::failpoint::Failure;
use inillucent_sim::media::MediaModel;
use inillucent_sim::sim_vfs::{CrashSnapshot, SimConfig, SimVfs};
use inillucent_tree::datum::OwnedDatum;
use inillucent_vfs::Vfs;

/// The page size these runs build at, matching `inillucent_engine::connect::PAGE_SIZE`.
const PAGE_SIZE: usize = 32_768;

/// How many frames the pool holds. Large enough that nothing evicts or
/// checkpoints on its own during a run this small.
const FRAMES: usize = 4_096;

/// The schema every run starts from.
///
/// `journal_mode` and `synchronous` are set through the pragmas rather than
/// through a constructor option: the new engine's `ImportedDatabase::create_on`
/// takes no journal configuration of its own, because a pragma is the one
/// place SQLite lets an application ask for either, and this engine answers
/// both for real (`crates/inillucent-engine/src/pragma.rs`).
const SCHEMA: &str = "PRAGMA journal_mode=wal;
     PRAGMA synchronous=full;
     CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER);
     CREATE INDEX t_b ON t(b);
     INSERT INTO t VALUES(1, 'one', 10);
     INSERT INTO t VALUES(2, 'two', 20);
     INSERT INTO t VALUES(3, 'three', 30);";

/// The transaction each run tries to commit on top of it.
const WORKLOAD: &str = "BEGIN;
     INSERT INTO t VALUES(4, 'four', 40);
     UPDATE t SET c = c + 1 WHERE a <= 2;
     DELETE FROM t WHERE a = 3;
     COMMIT;";

/// Returns a simulator with the pessimistic device model.
fn simulator(seed: u64) -> Arc<SimVfs> {
    Arc::new(SimVfs::new(SimConfig {
        seed,
        model: MediaModel::default(),
        ..SimConfig::default()
    }))
}

/// The path every run uses.
fn path() -> PathBuf {
    PathBuf::from("/sim/wal.db")
}

/// Creates the database on a simulator with nothing written to it yet.
fn create_fresh(vfs: Arc<dyn Vfs>) -> Result<ImportedDatabase, inillucent_base::DbError> {
    ImportedDatabase::create_on(vfs, path(), PAGE_SIZE, FRAMES)
}

/// Reopens a database a prior connection already built, reporting the failure
/// rather than panicking - a crash is exactly the case where this refuses.
fn reopen(vfs: Arc<dyn Vfs>) -> Result<ImportedDatabase, inillucent_base::DbError> {
    ImportedDatabase::open_on(vfs, path(), PAGE_SIZE, FRAMES)
}

/// Runs a script of one or more statements, stopping at the first failure.
fn run(engine: &mut ImportedDatabase, sql: &str) -> Result<(), inillucent_base::DbError> {
    let mut rest = sql;
    loop {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            return Ok(());
        }
        let consumed = engine.statement_length(trimmed)?;
        let Some(head) = trimmed.get(..consumed) else {
            return Ok(());
        };
        if head.trim().is_empty() {
            return Ok(());
        }
        engine.execute_any(head, &Params::new())?;
        rest = trimmed.get(consumed..).unwrap_or("");
    }
}

/// The rows a database holds, in a stable order.
fn try_contents(engine: &mut ImportedDatabase) -> Result<Vec<String>, inillucent_base::DbError> {
    let outcome = engine.execute_any("SELECT a, b, c FROM t ORDER BY a", &Params::new())?;
    let mut rows = Vec::new();
    for row in outcome.rows {
        let a = match row.first() {
            Some(OwnedDatum::Int(number)) => Some(*number),
            _ => None,
        };
        let b = match row.get(1) {
            Some(OwnedDatum::Text(bytes)) => Some(String::from_utf8_lossy(bytes).into_owned()),
            _ => None,
        };
        let c = match row.get(2) {
            Some(OwnedDatum::Int(number)) => Some(*number),
            _ => None,
        };
        rows.push(format!("{a:?}|{b:?}|{c:?}"));
    }
    Ok(rows)
}

/// Builds the database and returns the simulator holding it.
fn built(seed: u64) -> Arc<SimVfs> {
    let vfs = simulator(seed);
    let mut engine = create_fresh(Arc::clone(&vfs) as Arc<dyn Vfs>).expect("the connection opens");
    run(&mut engine, SCHEMA).expect("the schema builds");
    drop(engine);
    vfs
}

/// The two states a run may legitimately end in.
fn expected_states() -> (Vec<String>, Vec<String>) {
    let vfs = built(4242);
    let before = {
        let mut engine = reopen(Arc::clone(&vfs) as Arc<dyn Vfs>).expect("the connection opens");
        try_contents(&mut engine).expect("the query runs")
    };
    let after = {
        let mut engine = reopen(Arc::clone(&vfs) as Arc<dyn Vfs>).expect("the connection opens");
        run(&mut engine, WORKLOAD).expect("the workload commits");
        try_contents(&mut engine).expect("the query runs")
    };
    (before, after)
}

/// What reopening a crashed database produced.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Recovery {
    /// It opened and reported these rows.
    Rows(Vec<String>),
    /// It refused to be read, naming the damage.
    Broken(String),
}

/// Reopens what a crash left behind.
fn recovered(snapshot: &CrashSnapshot, seed: u64) -> Recovery {
    let vfs = Arc::new(SimVfs::recovered(
        SimConfig {
            seed,
            model: MediaModel::default(),
            ..SimConfig::default()
        },
        snapshot,
    ));
    match reopen(Arc::clone(&vfs) as Arc<dyn Vfs>).and_then(|mut engine| try_contents(&mut engine))
    {
        Ok(rows) => Recovery::Rows(rows),
        Err(failure) => Recovery::Broken(format!("{failure}")),
    }
}

/// Runs one campaign and returns its report.
///
/// `corruption_allowed` says whether the modelled failure can leave the
/// database file itself damaged, and `silent` whether the device reports the
/// failure to the writer. They come apart for exactly one failure and it is
/// worth naming: a write that stores half its bytes and returns success has
/// told the engine something false, so a commit built on it was reported to
/// the caller and cannot be honoured. The log's rolling checksum turns that
/// into a transaction that never happened rather than into a mixture, which is
/// the best any format can do with a device that lies - SQLite's durability
/// argument makes the same assumption. Every other failure keeps the full
/// promise: a commit that reported success is in the database afterwards.
fn campaign(
    name: &str,
    failure: Failure,
    tail: &str,
    corruption_allowed: bool,
    silent: bool,
) -> String {
    let (before, after) = expected_states();
    assert_ne!(before, after, "the workload has to change something");
    let mut report = String::new();
    let mut cuts = 0u64;
    let mut old = 0u64;
    let mut new = 0u64;
    let mut detected = 0u64;
    for nth in 1..=400u64 {
        let vfs = built(7000 + nth);
        let base = vfs.failpoints().sites_reached();
        vfs.failpoints()
            .fail_nth_call(base.saturating_add(nth), failure);
        // The connection is still open when the power goes. That is what a
        // crash is, and it matters here: a connection that had been closed
        // would have checkpointed its log and deleted it on the way out, so
        // every cut would be testing the checkpoint rather than the commit.
        let mut connection = reopen(Arc::clone(&vfs) as Arc<dyn Vfs>);
        let committed = match &mut connection {
            Ok(engine) => run(engine, WORKLOAD).is_ok() && run(engine, tail).is_ok(),
            Err(_) => false,
        };
        let reached = vfs.failpoints().sites_reached().saturating_sub(base);
        let snapshot = vfs.crash();
        drop(connection);
        if reached < nth {
            break;
        }
        cuts = cuts.saturating_add(1);
        let state = recovered(&snapshot, 8000 + nth);
        let verdict = match &state {
            Recovery::Rows(rows) if *rows == before => {
                old = old.saturating_add(1);
                "old"
            }
            Recovery::Rows(rows) if *rows == after => {
                new = new.saturating_add(1);
                "new"
            }
            Recovery::Broken(detail) => {
                assert!(
                    corruption_allowed,
                    "{name} cut {nth}: the database came back unreadable: {detail}"
                );
                detected = detected.saturating_add(1);
                "reported"
            }
            other => panic!(
                "{name} cut {nth}: neither state.\n  before: {before:?}\n  after:  {after:?}\n  got:    {other:?}"
            ),
        };
        assert!(
            !committed
                || verdict == "new"
                || silent
                || (corruption_allowed && verdict == "reported"),
            "{name} cut {nth}: the commit reported success and the database does not hold it"
        );
        report.push_str(&format!("{nth}\t{verdict}\t{committed}\n"));
    }
    assert!(cuts > 20, "{name}: only {cuts} cut points were reached");
    // **Not asserted for a short write.** Every commit now pads its own tail
    // to the next device sector boundary, in the same write and the same
    // sync as the commit record itself - see `inillucent_wal::writer`'s
    // `SECTOR_ALIGN` for the acknowledged-commit-loss defect that closes, and
    // `durability::a_full_disk_at_every_cut_point_is_recoverable` for where it
    // is measured directly. `Failure::ShortWrite` keeps a fixed *fraction* of
    // whatever a write asked to write - exactly half - so padding a small
    // commit's write large enough to close its sector boundary also makes
    // that write large enough that half of it always covers the commit's own
    // bytes in full. `tests/crash/wal-short-write.tsv` at the commit this
    // ticket found the defect in `HEAD` shows precisely that: `old` fell from
    // 7 of 24 cuts to 0, and every one of the cuts that used to land on the
    // schema's own small commits and produce `old` now produces `new`
    // instead - a commit that used to be small enough to lose entirely to a
    // short write keeps its own bytes now and loses only the padding after
    // it. That is the fix working, not a coverage regression: `old` was never
    // this campaign's own property to guarantee, it was evidence that a
    // small commit's tail *could* be lost, and closing that is the point.
    if failure != Failure::ShortWrite {
        assert!(old > 0, "{name}: no cut left the old state");
    }
    assert!(new > 0, "{name}: no cut left the new state");
    format!(
        "# {cuts} cuts: {old} old, {new} new, {detected} damaged and detected\ncut\tstate\tcommitted\n{report}"
    )
}

/// Every cut of a WAL commit leaves the database in one state or the other.
#[test]
fn every_cut_of_a_wal_commit_is_recoverable() {
    let report = campaign("commit", Failure::Crash, "SELECT 1", false, false);
    record("wal-commit.tsv", &report);
}

/// The same with a checkpoint at the end, so the cuts reach the copy back into
/// the database and the reset of the log.
#[test]
fn every_cut_of_a_wal_checkpoint_is_recoverable() {
    let report = campaign(
        "checkpoint",
        Failure::Crash,
        "PRAGMA wal_checkpoint(TRUNCATE)",
        false,
        false,
    );
    record("wal-checkpoint.tsv", &report);
}

/// A short write anywhere in a WAL commit leaves a recoverable database.
///
/// The log's own rolling checksum is what makes this the *easy* case for a
/// torn frame: a frame that was half written does not verify, so recovery
/// stops before it and the transaction is simply not there. What that costs is
/// the one exemption this file grants: the device reported success on a write
/// it did not do, so a commit this engine reported can come back as the old
/// state. It is never a mixture, and the database that comes back is always
/// one a reader can read - which is the whole of what a checksum can promise
/// against a device that lies about its writes.
#[test]
fn every_short_write_of_a_wal_commit_is_recoverable() {
    let report = campaign("short write", Failure::ShortWrite, "SELECT 1", true, true);
    record("wal-short-write.tsv", &report);
}

/// An I/O error at every cut point leaves a recoverable database.
#[test]
fn every_io_error_of_a_wal_commit_is_recoverable() {
    let report = campaign("io error", Failure::IoError, "SELECT 1", true, false);
    record("wal-io-error.tsv", &report);
}

/// Recovery is idempotent: crashing during one leaves something that recovers
/// to the same answer.
#[test]
fn a_crash_during_wal_recovery_is_idempotent() {
    let (before, after) = expected_states();
    let mut report = String::new();
    let mut covered = 0u64;
    for first in [12u64, 20, 28, 36] {
        let vfs = built(9000 + first);
        let base = vfs.failpoints().sites_reached();
        vfs.failpoints()
            .fail_nth_call(base.saturating_add(first), Failure::Crash);
        if let Ok(mut engine) = reopen(Arc::clone(&vfs) as Arc<dyn Vfs>) {
            let _ = run(&mut engine, WORKLOAD);
        }
        let snapshot = vfs.crash();
        // Recover it, crashing inside the recovery, and then recover *that*.
        for second in 1..=12u64 {
            let broken = Arc::new(SimVfs::recovered(
                SimConfig {
                    seed: 9500 + second,
                    model: MediaModel::default(),
                    ..SimConfig::default()
                },
                &snapshot,
            ));
            let base = broken.failpoints().sites_reached();
            broken
                .failpoints()
                .fail_nth_call(base.saturating_add(second), Failure::Crash);
            if let Ok(mut engine) = reopen(Arc::clone(&broken) as Arc<dyn Vfs>) {
                let _ = try_contents(&mut engine);
            }
            let twice = broken.crash();
            let state = recovered(&twice, 9900 + second);
            let verdict = match &state {
                Recovery::Rows(rows) if *rows == before => "old",
                Recovery::Rows(rows) if *rows == after => "new",
                Recovery::Broken(_) => "reported",
                other => panic!(
                    "cut {first}/{second}: neither state.\n  before: {before:?}\n  after:  {after:?}\n  got: {other:?}"
                ),
            };
            covered = covered.saturating_add(1);
            report.push_str(&format!("{first}\t{second}\t{verdict}\n"));
        }
    }
    assert!(covered >= 40, "only {covered} recovery cuts were reached");
    record(
        "wal-recovery-idempotent.tsv",
        &format!("first\tsecond\tstate\n{report}"),
    );
}

/// Writes a report into the checked-in crash schedules.
fn record(name: &str, body: &str) {
    let directory = inillucent_compat::workspace_root().join("tests/crash");
    let _ = std::fs::create_dir_all(&directory);
    let _ = std::fs::write(directory.join(name), body);
}

// A `diagnose_checkpoint_cut_29` test lived here, labelled "TASK1899-DIAG
// (temporary)" - a single cut point isolated from `every_cut_of_a_wal_
// checkpoint_is_recoverable`'s sweep, printing what recovery did with
// `eprintln!` rather than asserting anything. It confirmed cut 29 is real -
// `committed=false`, then `open_on failed: message=None detail=Some("database
// disk image is malformed")` - which is `every_cut_of_a_wal_checkpoint_is_
// recoverable` itself failing at "checkpoint cut 29", so nothing here proved
// anything the sweep does not already prove and assert on its own. Removed
// as a diagnostic that should never have shipped rather than kept as a
// second, weaker copy of the same case: `tests/inillucent-testing-tdd.md`
// rule 1.2, a test that cannot fail is worse than no test, and a `#[test]`
// with no assertion cannot.
//
// The defect it isolated is real and is not fixed here. `Database::checkpoint`
// (`crates/inillucent-pool/src/file.rs`) calls `write_free_map`
// unconditionally rather than only when the free map actually changed;
// `FreeMap` has no per-page dirty tracking to make that conditional on; and
// `Pool::install` (`crates/inillucent-pool/src/pool.rs`) writes the resulting
// page bytes into the pool without a WAL record or an LSN bump behind them.
// So a checkpoint's rewrite of free-map pages - even when their bytes are
// unchanged from the last checkpoint - has no log entry a crash mid-writeback
// could recover from, and `every_cut_of_a_wal_checkpoint_is_recoverable`'s cut
// 29 lands inside exactly that window. See the report this ticket published
// for which other currently-failing durability cases are the same defect and
// which are something else.
