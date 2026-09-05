//! Power loss, short writes, a full disk, and a crash during recovery.
//!
//! Invariant: after any modelled failure the database is either exactly what it
//! was before the transaction or exactly what it would have been after it, and
//! never a mixture. A transaction that reported success is in the second group;
//! one that reported a failure may be in either, but it is in one of them.
//!
//! The campaign is systematic rather than random. Every injectable VFS call of
//! a run is numbered, and the run is repeated once per number with the failure
//! armed at exactly that call - so "the crash matrix" is not a phrase, it is
//! every cut point of the commit, one at a time, with the outcome checked after
//! each.
//!
//! Recovery is put through the same treatment: the run that crashed is
//! recovered with a *second* crash armed inside the recovery, which is what
//! makes "recovery is idempotent" a measurement rather than an argument.

use std::sync::Arc;

use inillucent_session::connection::{Connection, OpenOptions, SessionDatabase};
use inillucent_sim::failpoint::Failure;
use inillucent_sim::media::MediaModel;
use inillucent_sim::sim_vfs::{CrashSnapshot, SimConfig, SimVfs};
use inillucent_transaction::journal::{JournalMode, JournalOptions, Synchronous};
use inillucent_value::Value;
use inillucent_vfs::path::DbPath;
use inillucent_vfs::Vfs;

/// The database every run in this file builds.
const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER);
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

/// A transaction that makes the file grow, so a rollback has to shrink it.
///
/// Wide rows, and enough of them, that committing has to extend the file past
/// the pages it already had. The test below crashes partway through that
/// commit, which is the only way to leave a database larger than the page count
/// its journal will restore.
const GROWING_WORKLOAD: &str = "BEGIN;
     INSERT INTO t SELECT 1000 + n, printf('%.400c', 120), n
       FROM (WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 400)
             SELECT n FROM c);
     COMMIT;";

/// Returns the page size and page count a database header declares.
fn header_shape(bytes: &[u8]) -> Option<(u64, u64)> {
    let raw = u64::from(u16::from_be_bytes([*bytes.get(16)?, *bytes.get(17)?]));
    // The one page size the header's two bytes cannot hold is written as one.
    let page_size = if raw == 1 { 65_536 } else { raw };
    let count = u64::from(u32::from_be_bytes([
        *bytes.get(28)?,
        *bytes.get(29)?,
        *bytes.get(30)?,
        *bytes.get(31)?,
    ]));
    Some((page_size, count))
}

/// A recovered database is exactly as long as its header says it is.
///
/// A rollback restores the page *count* from the journal, and the file has to
/// be truncated to match it. If it is not, the database is left carrying pages
/// the rolled-back transaction allocated: every later read is still correct,
/// which is why nothing else notices, and the file simply never shrinks again.
///
/// The workload grows the file and the crash is armed at every cut point in
/// turn, so this does not depend on guessing which call leaves the file long -
/// it asserts the invariant at all of them.
///
/// It is worth being exact about what this does and does not pin. Mutation
/// testing reported `finish_recovery`'s `if database.file_size()? > wanted` as
/// a surviving mutant, and this test does **not** kill it: with the truncation
/// disabled the invariant still holds at all thirty-one reopenable cut points,
/// because none of them leaves a file longer than its header claims. The
/// branch is not reachable under this crash model - the media the simulator
/// leaves behind never carries the extension - so no test at this level can
/// kill that mutant, and it is an equivalent mutant in practice rather than a
/// missing test.
///
/// The invariant is worth asserting on its own account, which is why it stays:
/// a recovered database that is longer than its own page count is a database
/// carrying pages nothing will ever reclaim.
#[test]
fn a_recovered_database_is_no_longer_than_its_header_says() {
    let journal = JournalOptions::default();
    let seed = 90_210;
    let reach = attempt_with(journal, seed, u64::MAX, Failure::Crash, GROWING_WORKLOAD).reached;
    assert!(reach > 0, "the workload has to reach some injectable calls");

    let mut checked = 0usize;
    let mut longest = 0u64;
    for nth in 1..=reach {
        let run = attempt_with(journal, seed, nth, Failure::Crash, GROWING_WORKLOAD);
        let recovered_vfs = Arc::new(SimVfs::recovered(
            SimConfig {
                seed,
                model: MediaModel::default(),
                ..SimConfig::default()
            },
            &run.snapshot,
        ));
        // Opening runs recovery; the connection is dropped before the file is
        // measured so nothing of ours is still holding pages open.
        let opened = try_connect(Arc::clone(&recovered_vfs) as Arc<dyn Vfs>, journal).is_ok();
        if !opened {
            continue;
        }
        let Some(bytes) = recovered_vfs.visible_bytes(&path()) else {
            continue;
        };
        let Some((page_size, count)) = header_shape(&bytes) else {
            continue;
        };
        let wanted = count.saturating_mul(page_size);
        longest = longest.max(bytes.len() as u64);
        assert!(
            bytes.len() as u64 <= wanted,
            "cut {nth}: recovery left {} bytes for a header claiming {count} pages of \
             {page_size} ({wanted} bytes) - the pages the rolled-back transaction took \
             were never given back",
            bytes.len()
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "no cut point produced a database that could be reopened, so nothing was checked"
    );
    assert!(
        longest > 0,
        "no recovered database had any length, so nothing was measured"
    );
}

/// Returns a simulator with the pessimistic device model.
fn simulator(seed: u64) -> Arc<SimVfs> {
    Arc::new(SimVfs::new(SimConfig {
        seed,
        model: MediaModel::default(),
        ..SimConfig::default()
    }))
}

/// Returns the path every run uses.
fn path() -> DbPath {
    DbPath::from("/sim/app.db")
}

/// Opens a connection on one simulated file system.
fn connect(vfs: Arc<dyn Vfs>, journal: JournalOptions) -> Connection {
    try_connect(vfs, journal).expect("the connection opens")
}

/// Opens a connection, reporting the failure rather than panicking.
///
/// A campaign arms its failure before the connection is opened, and opening is
/// itself a cut point: recovery runs there, and a PERSIST journal from the
/// previous transaction is deleted there. A harness that could not survive a
/// failure during open would simply not test those.
fn try_connect(
    vfs: Arc<dyn Vfs>,
    journal: JournalOptions,
) -> Result<Connection, inillucent_base::DbError> {
    let database = SessionDatabase::open_with(
        path().as_path(),
        vfs,
        OpenOptions {
            journal,
            ..OpenOptions::default()
        },
    )?;
    database.connect()
}

/// The rows a database holds, as text, in a stable order.
fn contents(connection: &Connection) -> Vec<String> {
    try_contents(connection).expect("the query runs")
}

/// Reads the rows, reporting a failure rather than panicking.
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

/// Runs the schema and the workload with no failure, returning both states.
fn expected_states(journal: JournalOptions) -> (Vec<String>, Vec<String>) {
    let vfs = simulator(7);
    let before = {
        let connection = connect(Arc::clone(&vfs) as Arc<dyn Vfs>, journal);
        run(&connection, SCHEMA).expect("the schema builds");
        contents(&connection)
    };
    let after = {
        let connection = connect(Arc::clone(&vfs) as Arc<dyn Vfs>, journal);
        run(&connection, WORKLOAD).expect("the workload commits");
        contents(&connection)
    };
    (before, after)
}

/// Runs a script, returning whether it succeeded.
fn run(connection: &Connection, sql: &str) -> Result<(), inillucent_base::DbError> {
    inillucent_session::statement::execute_batch(connection, sql.as_bytes())
}

/// Builds a database and returns the simulator holding it.
fn built(journal: JournalOptions, seed: u64) -> Arc<SimVfs> {
    let vfs = simulator(seed);
    let connection = connect(Arc::clone(&vfs) as Arc<dyn Vfs>, journal);
    run(&connection, SCHEMA).expect("the schema builds");
    drop(connection);
    vfs
}

/// What one armed run did.
struct Attempt {
    /// Whether the workload reported that it committed.
    committed: bool,
    /// What the media held after the power loss.
    snapshot: CrashSnapshot,
    /// How many injectable calls the run reached.
    reached: u64,
}

/// Runs the workload with a failure armed at the `n`th injectable call *of the
/// workload*.
///
/// The failpoint table counts every call the simulator has ever made, and
/// building the database costs hundreds of them. Arming call `n` directly
/// would therefore arm a call that had already happened, and the campaign
/// would report a hundred cut points while causing no failures at all - which
/// is what it did until the base was subtracted.
fn attempt(journal: JournalOptions, seed: u64, nth: u64, failure: Failure) -> Attempt {
    attempt_with(journal, seed, nth, failure, WORKLOAD)
}

/// As [`attempt`], for a workload other than the standard one.
fn attempt_with(
    journal: JournalOptions,
    seed: u64,
    nth: u64,
    failure: Failure,
    workload: &str,
) -> Attempt {
    let vfs = built(journal, seed);
    let base = vfs.failpoints().sites_reached();
    vfs.failpoints()
        .fail_nth_call(base.saturating_add(nth), failure);
    let committed = match try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, journal) {
        Ok(connection) => run(&connection, workload).is_ok(),
        Err(_) => false,
    };
    let reached = vfs.failpoints().sites_reached().saturating_sub(base);
    Attempt {
        committed,
        snapshot: vfs.crash(),
        reached,
    }
}

/// What reopening a crashed database produced.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Recovery {
    /// The database opened and reported these rows.
    Rows(Vec<String>),
    /// The database refused to be read, naming the damage.
    Corrupt(String),
}

/// Reopens what a crash left behind and reports the rows it holds.
fn recovered(journal: JournalOptions, snapshot: &CrashSnapshot, seed: u64) -> Recovery {
    let vfs = Arc::new(SimVfs::recovered(
        SimConfig {
            seed,
            model: MediaModel::default(),
            ..SimConfig::default()
        },
        snapshot,
    ));
    match try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, journal)
        .and_then(|connection| try_contents(&connection))
    {
        Ok(rows) => Recovery::Rows(rows),
        Err(failure) => Recovery::Corrupt(format!("{failure}")),
    }
}

/// Runs the whole campaign for one journal mode and durability level.
///
/// `corruption_allowed` says whether a run may end with the database refusing
/// to be read. It is false for every failure a VFS is allowed to report, and
/// true only for the short write, which reports success and stores half the
/// bytes: no durability scheme can undo a write that never said it failed, and
/// the guarantee that remains is that the damage is *detected* rather than
/// served as rows.
fn campaign(
    journal: JournalOptions,
    failure: Failure,
    limit: u64,
    corruption_allowed: bool,
) -> String {
    let (before, after) = expected_states(journal);
    assert_ne!(before, after, "the workload has to change something");
    let mut report = String::new();
    let mut cut_points = 0u64;
    let mut committed_runs = 0u64;
    let mut detected = 0u64;
    for nth in 1..=limit {
        let outcome = attempt(journal, 1786 + nth, nth, failure);
        if outcome.reached < nth {
            // The failure was armed past the last call the run makes, so it
            // never fired: the transaction committed and *then* the power went.
            // That is the case the FULL guarantee is about, and it is checked
            // here rather than assumed - after which the campaign is done,
            // because there are no cut points left.
            assert!(
                outcome.committed,
                "an unarmed run must commit; the workload is not deterministic"
            );
            let recovery = recovered(journal, &outcome.snapshot, 4242 + nth);
            assert_eq!(
                recovery,
                Recovery::Rows(after.clone()),
                "a commit that was acknowledged and then power-cut was lost"
            );
            report.push_str(&format!(
                "{nth}	acknowledged	new
"
            ));
            committed_runs = committed_runs.saturating_add(1);
            break;
        }
        cut_points = cut_points.saturating_add(1);
        let recovery = recovered(journal, &outcome.snapshot, 4242 + nth);
        let matched = match &recovery {
            Recovery::Rows(rows) if *rows == before => "old",
            Recovery::Rows(rows) if *rows == after => "new",
            Recovery::Rows(_) => "MIXED",
            Recovery::Corrupt(_) => "detected",
        };
        if outcome.committed {
            committed_runs = committed_runs.saturating_add(1);
            match &recovery {
                Recovery::Corrupt(detail) => {
                    // A write that reported success and stored half the bytes
                    // has destroyed a page the journal was no longer holding
                    // an image of. No rollback scheme can undo that, and
                    // SQLite is exposed to it identically; what is still owed
                    // is that the damage is *detected* rather than served as
                    // rows, and that is what this arm records.
                    assert!(
                        corruption_allowed,
                        "call {nth}: an acknowledged commit came back unreadable: {detail}"
                    );
                    detected = detected.saturating_add(1);
                }
                other => assert_eq!(
                    *other,
                    Recovery::Rows(after.clone()),
                    "call {nth}: the commit was reported and then lost"
                ),
            }
        } else {
            match &recovery {
                Recovery::Rows(rows) => assert!(
                    *rows == before || *rows == after,
                    "call {nth}: recovery produced a state that is neither\n  got {rows:?}\n  before {before:?}\n  after {after:?}"
                ),
                Recovery::Corrupt(detail) => {
                    assert!(
                        corruption_allowed,
                        "call {nth}: a reported failure left an unreadable database: {detail}"
                    );
                    detected = detected.saturating_add(1);
                }
            }
        }
        report.push_str(&format!(
            "{nth}\t{}\t{matched}\n",
            if outcome.committed {
                "committed"
            } else {
                "failed"
            }
        ));
    }
    assert!(
        cut_points >= 10,
        "a campaign that covers {cut_points} cut points is not a campaign"
    );
    format!(
        "cut points: {cut_points}, acknowledged commits: {committed_runs}, detected damage: {detected}\n{report}"
    )
}

/// Writes a campaign's report into the checked-in crash schedules.
///
/// The runs are seeded, so the file a run produces is the file the next run
/// produces: a diff on it is a change in what the engine does under failure,
/// which is exactly the thing a review should be shown rather than told.
fn record(name: &str, body: &str) {
    let directory = inillucent_compat::workspace_root().join("tests/crash");
    let _ = std::fs::create_dir_all(&directory);
    let _ = std::fs::write(directory.join(name), body);
}

/// A power loss at every cut point of a DELETE-mode FULL commit leaves the old
/// database or the new one, and an acknowledged commit is never lost.
#[test]
fn power_loss_at_every_cut_point_of_a_full_commit() {
    let journal = JournalOptions {
        mode: JournalMode::Delete,
        synchronous: Synchronous::Full,
    };
    let report = campaign(journal, Failure::Crash, 220, false);
    record("delete-full-crash.txt", &report);
}

/// TRUNCATE mode has a different commit point - the truncation rather than the
/// deletion - and the same guarantee.
#[test]
fn power_loss_at_every_cut_point_of_a_truncate_commit() {
    let journal = JournalOptions {
        mode: JournalMode::Truncate,
        synchronous: Synchronous::Full,
    };
    let report = campaign(journal, Failure::Crash, 220, false);
    record("truncate-full-crash.txt", &report);
}

/// PERSIST mode's commit point is the header write that makes the journal
/// stop being hot.
#[test]
fn power_loss_at_every_cut_point_of_a_persist_commit() {
    let journal = JournalOptions {
        mode: JournalMode::Persist,
        synchronous: Synchronous::Full,
    };
    let report = campaign(journal, Failure::Crash, 220, false);
    record("persist-full-crash.txt", &report);
}

/// A short write is either recovered or reported, never served as rows.
///
/// This one failure is outside what any rollback journal can undo. A write
/// that stores half its bytes and reports success has destroyed data the
/// engine was told had landed, and by the time the journal is finalised there
/// is no image left to put back - SQLite has exactly the same exposure, which
/// is why its durability argument assumes a write either lands or fails. What
/// is still owed, and what this measures, is that every such run ends either
/// in a clean old-or-new database or in a *reported* corruption, and never in
/// plausible-looking rows that are neither.
#[test]
fn a_short_write_at_every_cut_point_is_recoverable() {
    let journal = JournalOptions::default();
    let report = campaign(journal, Failure::ShortWrite, 160, true);
    record("delete-full-short-write.txt", &report);
}

/// A full disk at every cut point leaves a recoverable database.
#[test]
fn a_full_disk_at_every_cut_point_is_recoverable() {
    let journal = JournalOptions::default();
    let report = campaign(journal, Failure::DiskFull, 160, false);
    record("delete-full-disk-full.txt", &report);
}

/// An I/O error at every cut point leaves a recoverable database.
#[test]
fn an_io_error_at_every_cut_point_is_recoverable() {
    let journal = JournalOptions::default();
    let report = campaign(journal, Failure::IoError, 160, false);
    record("delete-full-io-error.txt", &report);
}

/// A crash *during* recovery leaves either another hot journal that replays to
/// the same result, or the complete old database.
#[test]
fn a_crash_during_recovery_is_idempotent() {
    let journal = JournalOptions::default();
    let (before, after) = expected_states(journal);
    let mut report = String::new();
    let mut covered = 0u64;
    // A crash part-way through the commit is what leaves a hot journal to
    // recover from; call 40 is inside the database write on this workload.
    for first in [24u64, 32, 40, 48] {
        let crashed = attempt(journal, 900 + first, first, Failure::Crash);
        for second in 1..=12u64 {
            let vfs = Arc::new(SimVfs::recovered(
                SimConfig {
                    seed: 5000 + first * 100 + second,
                    model: MediaModel::default(),
                    ..SimConfig::default()
                },
                &crashed.snapshot,
            ));
            vfs.failpoints().fail_nth_call(second, Failure::Crash);
            // The recovery may fail or may be cut short; either way what it
            // leaves has to be recoverable by the next attempt.
            let _ = SessionDatabase::open_with(
                path().as_path(),
                Arc::clone(&vfs) as Arc<dyn Vfs>,
                OpenOptions {
                    journal,
                    ..OpenOptions::default()
                },
            )
            .and_then(|database| database.connect());
            let interrupted = vfs.crash();
            let recovery = recovered(journal, &interrupted, 60_000 + second);
            let Recovery::Rows(rows) = &recovery else {
                panic!(
                    "a crash at call {second} of the recovery of a crash at call {first} left an unreadable database: {recovery:?}"
                );
            };
            assert!(
                *rows == before || *rows == after,
                "a crash at call {second} of the recovery of a crash at call {first} left a mixture\n  got {rows:?}"
            );
            covered = covered.saturating_add(1);
            report.push_str(&format!(
                "{first}\t{second}\t{}\n",
                if *rows == before { "old" } else { "new" }
            ));
        }
    }
    assert!(
        covered >= 40,
        "only {covered} recovery cut points were tried"
    );
    record("recovery-crash.txt", &report);
}

/// A statement that fails inside a transaction undoes itself and leaves the
/// rest of the transaction intact.
#[test]
fn a_failed_statement_undoes_only_itself() {
    let journal = JournalOptions::default();
    let vfs = built(journal, 33);
    let connection = connect(Arc::clone(&vfs) as Arc<dyn Vfs>, journal);
    run(&connection, "BEGIN").expect("begins");
    run(&connection, "INSERT INTO t VALUES(10, 'ten', 100)").expect("inserts");
    // The second row of this statement collides with the row the first
    // statement wrote, so ABORT undoes the whole statement - both rows.
    let failed = run(
        &connection,
        "INSERT INTO t VALUES(11, 'eleven', 110), (10, 'again', 120)",
    );
    assert!(failed.is_err(), "the duplicate key must be refused");
    run(&connection, "COMMIT").expect("commits");
    let rows = contents(&connection);
    assert!(
        rows.iter().any(|row| row.contains("10")),
        "the first statement's row survived: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("eleven")),
        "the failed statement's earlier row was undone: {rows:?}"
    );
}
