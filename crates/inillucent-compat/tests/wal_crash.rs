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

use std::sync::Arc;

use inillucent_session::connection::{Connection, OpenOptions, SessionDatabase};
use inillucent_sim::failpoint::Failure;
use inillucent_sim::media::MediaModel;
use inillucent_sim::sim_vfs::{CrashSnapshot, SimConfig, SimVfs};
use inillucent_transaction::journal::{JournalMode, JournalOptions, Synchronous};
use inillucent_value::Value;
use inillucent_vfs::path::DbPath;
use inillucent_vfs::Vfs;

/// The schema every run starts from.
const SCHEMA: &str = "PRAGMA journal_mode=wal;
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
fn path() -> DbPath {
    DbPath::from("/sim/wal.db")
}

/// The options a WAL connection opens with.
fn options() -> JournalOptions {
    JournalOptions {
        mode: JournalMode::Wal,
        synchronous: Synchronous::Full,
    }
}

/// Opens a connection, reporting the failure rather than panicking.
fn try_connect(vfs: Arc<dyn Vfs>) -> Result<Connection, inillucent_base::DbError> {
    let database = SessionDatabase::open_with(
        path().as_path(),
        vfs,
        OpenOptions {
            journal: options(),
            ..OpenOptions::default()
        },
    )?;
    database.connect()
}

/// Runs a script, reporting whether it succeeded.
fn run(connection: &Connection, sql: &str) -> Result<(), inillucent_base::DbError> {
    inillucent_session::statement::execute_batch(connection, sql.as_bytes())
}

/// The rows a database holds, in a stable order.
fn try_contents(connection: &Connection) -> Result<Vec<String>, inillucent_base::DbError> {
    let (mut statement, _) = inillucent_session::statement::Statement::prepare(
        connection,
        b"SELECT a, b, c FROM t ORDER BY a",
    )?;
    let mut rows = Vec::new();
    while statement.step()? {
        let row = statement.row();
        rows.push(format!(
            "{:?}|{:?}|{:?}",
            row.first().and_then(Value::as_integer),
            row.get(1)
                .and_then(|value| value.as_text().map(|text| text.utf8_bytes().into_owned())),
            row.get(2).and_then(Value::as_integer),
        ));
    }
    Ok(rows)
}

/// Builds the database and returns the simulator holding it.
fn built(seed: u64) -> Arc<SimVfs> {
    let vfs = simulator(seed);
    let connection = try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>).expect("the connection opens");
    run(&connection, SCHEMA).expect("the schema builds");
    drop(connection);
    vfs
}

/// The two states a run may legitimately end in.
fn expected_states() -> (Vec<String>, Vec<String>) {
    let vfs = built(4242);
    let before = {
        let connection =
            try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>).expect("the connection opens");
        try_contents(&connection).expect("the query runs")
    };
    let after = {
        let connection =
            try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>).expect("the connection opens");
        run(&connection, WORKLOAD).expect("the workload commits");
        try_contents(&connection).expect("the query runs")
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
    match try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>).and_then(|connection| {
        let rows = try_contents(&connection)?;
        Ok(rows)
    }) {
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
        let connection = try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>);
        let committed = match &connection {
            Ok(connection) => run(connection, WORKLOAD).is_ok() && run(connection, tail).is_ok(),
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
    assert!(old > 0, "{name}: no cut left the old state");
    assert!(new > 0, "{name}: no cut left the new state");
    format!(
        "# {cuts} cuts: {old} old, {new} new, {detected} reported\ncut\tstate\treported\n{report}"
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
        if let Ok(connection) = try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>) {
            let _ = run(&connection, WORKLOAD);
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
            if let Ok(connection) = try_connect(Arc::clone(&broken) as Arc<dyn Vfs>) {
                let _ = try_contents(&connection);
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
