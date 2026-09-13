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
//!
//! Re-pointed from the old engine (`inillucent-session`) onto the new one
//! (`inillucent-engine`). The campaign machinery - `SimVfs`, its failpoints, its
//! crash snapshots - lives below both engines and is unchanged; what moved is
//! how a database is opened on a chosen `Vfs` and how a statement runs. The old
//! engine took an `OpenOptions { journal: JournalOptions { mode, synchronous } }`
//! at open time; the new engine's `PRAGMA journal_mode` and `PRAGMA synchronous`
//! are real switches now (`inillucent-engine`'s own `pragma` module says so), so
//! the journal configuration each campaign wants is set as SQL right after
//! opening rather than passed to the constructor. This is the same pattern
//! `new_engine_recovery_shapes.rs` already uses to drive `ImportedDatabase`
//! directly on a `SimVfs`: there is no connection/session layer between them,
//! because a crash campaign only ever has one writer at a time.

use std::path::PathBuf;
use std::sync::Arc;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_sim::failpoint::Failure;
use inillucent_sim::media::MediaModel;
use inillucent_sim::sim_vfs::{CrashSnapshot, SimConfig, SimVfs};
use inillucent_tree::datum::OwnedDatum;
use inillucent_vfs::path::DbPath;
use inillucent_vfs::{AccessMode, Vfs};

/// The page size these tests build at, and the frames the pool holds.
///
/// Large enough that nothing is evicted and no checkpoint happens on its own
/// mid-workload, which is what leaves the whole workload reachable in the log.
const PAGE_SIZE: usize = 4_096;
const FRAMES: usize = 8_192;

/// The database every run in this file builds.
const SCHEMA: [&str; 4] = [
    "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER)",
    "CREATE INDEX t_b ON t(b)",
    "INSERT INTO t VALUES(1, 'one', 10), (2, 'two', 20), (3, 'three', 30)",
    "",
];

/// The transaction each run tries to commit on top of it.
const WORKLOAD: [&str; 5] = [
    "BEGIN",
    "INSERT INTO t VALUES(4, 'four', 40)",
    "UPDATE t SET c = c + 1 WHERE a <= 2",
    "DELETE FROM t WHERE a = 3",
    "COMMIT",
];

/// A transaction that makes the file grow, so a rollback has to shrink it.
///
/// Wide rows, and enough of them, that committing has to extend the file past
/// the pages it already had. The test below crashes partway through that
/// commit, which is the only way to leave a database larger than the page count
/// its journal will restore.
const GROWING_WORKLOAD: [&str; 2] = [
    "BEGIN",
    "INSERT INTO t SELECT 1000 + n, printf('%.400c', 120), n \
       FROM (WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 400) \
             SELECT n FROM c)",
];
/// `GROWING_WORKLOAD`'s commit, kept apart so a crash can be armed anywhere in
/// the insert without also covering the commit itself.
const GROWING_COMMIT: &str = "COMMIT";

/// Runs one statement, using the engine's write path directly.
///
/// @param engine - the database
/// @param sql - the statement
fn exec(engine: &mut ImportedDatabase, sql: &str) -> Result<(), inillucent_base::DbError> {
    engine.execute_any(sql, &Params::new())?;
    Ok(())
}

/// Runs every statement of a script in order, stopping at the first failure.
///
/// @param engine - the database
/// @param script - the statements, in order
fn run_script(
    engine: &mut ImportedDatabase,
    script: &[&str],
) -> Result<(), inillucent_base::DbError> {
    for statement in script {
        if statement.is_empty() {
            continue;
        }
        exec(engine, statement)?;
    }
    Ok(())
}

/// Sets the journal mode and durability level a campaign runs under.
///
/// Fallible on purpose: unlike the retired engine, which took the mode as a
/// constructor option, this one applies it as `PRAGMA` statements after
/// opening (see the module comment) - and a campaign's `open()` is called with
/// a failure already armed, so the switch itself can land on the armed call.
/// That is a legitimate cut point of "reopen and get back to work", not a
/// harness error, so it is propagated like any other statement failure rather
/// than unwrapped - `open()` folds it into the same `Err` its caller already
/// treats as "this attempt never got to run the workload".
///
/// @param engine - the database
/// @param mode - `delete`, `truncate` or `persist`
/// @param synchronous - `full` or another `PRAGMA synchronous` spelling
fn set_journal(
    engine: &mut ImportedDatabase,
    mode: &str,
    synchronous: &str,
) -> Result<(), inillucent_base::DbError> {
    exec(engine, &format!("PRAGMA journal_mode = {mode}"))?;
    exec(engine, &format!("PRAGMA synchronous = {synchronous}"))?;
    Ok(())
}

/// One journal configuration a campaign runs under.
#[derive(Clone, Copy)]
struct Journal {
    mode: &'static str,
    synchronous: &'static str,
}

impl Journal {
    /// The configuration `JournalOptions::default()` used to mean: DELETE mode,
    /// FULL synchronous.
    const DEFAULT: Journal = Journal {
        mode: "delete",
        synchronous: "full",
    };

    /// No supplementary rollback journal at all - the write-ahead log is the
    /// only durability mechanism in force.
    ///
    /// **The no-steal campaigns need this, and `DEFAULT` would tell them
    /// nothing.** Every non-`off` mode, `wal` included, still takes a `delete`
    /// rollback journal beside the log (`journal_for` in
    /// `inillucent-engine/src/lib.rs`), which journals a pre-image before *any*
    /// page writeback, a checkpoint's or an ordinary eviction's, and
    /// `replay_hot_journal` puts every such page back on the next open
    /// regardless of what the log's own recovery would have done. That
    /// already undoes an evicted, uncommitted page with no help from no-steal
    /// at all - measured directly: [`a_transaction_the_evictor_writes_back_never_survives_uncommitted`]
    /// passed under `DEFAULT` whether or not `Pool::holds_uncommitted` was
    /// armed. `off` removes that safety net, so what is left protecting an
    /// uncommitted row is exactly the mechanism this file's no-steal campaigns
    /// are about.
    const NO_ROLLBACK_JOURNAL: Journal = Journal {
        mode: "off",
        synchronous: "full",
    };
}

/// Returns the page size and page count the recovered database's own meta
/// page reports.
///
/// Re-pointed from the SQLite file header (bytes 16-17 for the page size,
/// 28-31 for the page count) onto the new engine's own format, which is not
/// SQLite's: `inillucent-pool`'s `meta` module keeps two candidate meta
/// pages, `META_PAGE` (page 0) and `SHADOW_PAGE` (page 1), and a reader
/// believes whichever has a valid checksum and the higher generation -
/// `Meta::choose` is that rule, and this reads by the same one rather than
/// assuming page 0 is current. That matters here specifically: a checkpoint
/// writes the *other* page first (see `meta.rs`'s module comment), so which
/// page is current alternates every checkpoint, and a header reader that
/// always trusted page 0 would report a stale page count on every other
/// checkpoint - not a defect in the engine, but a wrong question from the
/// test.
fn header_shape(bytes: &[u8]) -> Option<(u64, u64)> {
    let primary = bytes.get(..PAGE_SIZE)?;
    let shadow = bytes.get(PAGE_SIZE..PAGE_SIZE.saturating_mul(2))?;
    let meta = inillucent_pool::meta::Meta::choose(primary, shadow).ok()?;
    Some((u64::from(meta.page_size), meta.page_count))
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
#[test]
fn a_recovered_database_is_no_longer_than_its_header_says() {
    let journal = Journal::DEFAULT;
    let seed = 90_210;
    let reach = attempt_growing(journal, seed, u64::MAX).reached;
    assert!(reach > 0, "the workload has to reach some injectable calls");

    let mut checked = 0usize;
    let mut longest = 0u64;
    for nth in 1..=reach {
        let run = attempt_growing(journal, seed, nth);
        let recovered_vfs = Arc::new(SimVfs::recovered(
            SimConfig {
                seed,
                model: MediaModel::default(),
                ..SimConfig::default()
            },
            &run.snapshot,
        ));
        // Opening runs recovery; the handle is dropped before the file is
        // measured so nothing of ours is still holding pages open.
        let opened = ImportedDatabase::open_on(
            Arc::clone(&recovered_vfs) as Arc<dyn Vfs>,
            path(),
            PAGE_SIZE,
            FRAMES,
        )
        .is_ok();
        if !opened {
            continue;
        }
        let Some(bytes) = recovered_vfs.visible_bytes(&db_path()) else {
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

/// Returns the path every run uses.
fn path() -> PathBuf {
    PathBuf::from("app.db")
}

/// Returns `path()` the way the VFS layer names it.
fn db_path() -> DbPath {
    DbPath::new(path().to_string_lossy().as_ref())
}

/// Returns a simulator with the pessimistic device model.
fn simulator(seed: u64) -> Arc<SimVfs> {
    Arc::new(SimVfs::new(SimConfig {
        seed,
        model: MediaModel::default(),
        ..SimConfig::default()
    }))
}

/// Opens a database on one simulated file system, with the campaign's journal
/// settings applied.
fn open(vfs: Arc<dyn Vfs>, journal: Journal) -> Result<ImportedDatabase, inillucent_base::DbError> {
    let mut engine = ImportedDatabase::open_on(vfs, path(), PAGE_SIZE, FRAMES)?;
    set_journal(&mut engine, journal.mode, journal.synchronous)?;
    Ok(engine)
}

/// Creates a fresh database on one simulated file system, with the campaign's
/// journal settings applied.
///
/// Never called with a failure armed - `built()` and `expected_states()` both
/// use it before any campaign failpoint is set - so the settings are expected
/// to apply outright.
fn create(vfs: Arc<dyn Vfs>, journal: Journal) -> ImportedDatabase {
    let mut engine = ImportedDatabase::create_on(vfs, path(), PAGE_SIZE, FRAMES)
        .expect("the database is created");
    set_journal(&mut engine, journal.mode, journal.synchronous)
        .expect("journal settings apply on a fresh, unarmed database");
    engine
}

/// The rows a database holds, as text, in a stable order.
fn contents(engine: &mut ImportedDatabase) -> Vec<String> {
    try_contents(engine).expect("the query runs")
}

/// Reads the rows, reporting a failure rather than panicking.
fn try_contents(engine: &mut ImportedDatabase) -> Result<Vec<String>, inillucent_base::DbError> {
    let outcome = engine.execute_any("SELECT a, b, c FROM t ORDER BY a", &Params::new())?;
    Ok(outcome
        .rows
        .iter()
        .map(|row| {
            format!(
                "{:?}|{:?}|{:?}",
                as_integer(row.first()),
                as_text(row.get(1)),
                as_integer(row.get(2)),
            )
        })
        .collect())
}

/// Returns an integer datum as `Option<i64>`, matching the old `Value::as_integer`.
fn as_integer(value: Option<&OwnedDatum>) -> Option<i64> {
    match value {
        Some(OwnedDatum::Int(value)) => Some(*value),
        _ => None,
    }
}

/// Returns a text datum as `Option<String>`, matching the old `Value::as_text`.
fn as_text(value: Option<&OwnedDatum>) -> Option<String> {
    match value {
        Some(OwnedDatum::Text(bytes)) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

/// Runs the schema and the workload with no failure, returning both states.
fn expected_states(journal: Journal) -> (Vec<String>, Vec<String>) {
    let vfs = simulator(7);
    let before = {
        let mut engine = create(Arc::clone(&vfs) as Arc<dyn Vfs>, journal);
        run_script(&mut engine, &SCHEMA).expect("the schema builds");
        contents(&mut engine)
    };
    let after = {
        let mut engine = open(Arc::clone(&vfs) as Arc<dyn Vfs>, journal).expect("it reopens");
        run_script(&mut engine, &WORKLOAD).expect("the workload commits");
        contents(&mut engine)
    };
    (before, after)
}

/// Builds a database and returns the simulator holding it.
fn built(journal: Journal, seed: u64) -> Arc<SimVfs> {
    let vfs = simulator(seed);
    let mut engine = create(Arc::clone(&vfs) as Arc<dyn Vfs>, journal);
    run_script(&mut engine, &SCHEMA).expect("the schema builds");
    drop(engine);
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
fn attempt(journal: Journal, seed: u64, nth: u64, failure: Failure) -> Attempt {
    attempt_with(journal, seed, nth, failure, &WORKLOAD)
}

/// As [`attempt`], for the growing workload, which crashes only inside the
/// insert - the commit is armed separately once the insert itself is done.
fn attempt_growing(journal: Journal, seed: u64, nth: u64) -> Attempt {
    let vfs = built(journal, seed);
    let base = vfs.failpoints().sites_reached();
    vfs.failpoints()
        .fail_nth_call(base.saturating_add(nth), Failure::Crash);
    let committed = match open(Arc::clone(&vfs) as Arc<dyn Vfs>, journal) {
        Ok(mut engine) => run_script(&mut engine, &GROWING_WORKLOAD)
            .and_then(|()| exec(&mut engine, GROWING_COMMIT))
            .is_ok(),
        Err(_) => false,
    };
    let reached = vfs.failpoints().sites_reached().saturating_sub(base);
    Attempt {
        committed,
        snapshot: vfs.crash(),
        reached,
    }
}

/// As [`attempt`], for a workload other than the standard one.
fn attempt_with(
    journal: Journal,
    seed: u64,
    nth: u64,
    failure: Failure,
    workload: &[&str],
) -> Attempt {
    let vfs = built(journal, seed);
    let base = vfs.failpoints().sites_reached();
    vfs.failpoints()
        .fail_nth_call(base.saturating_add(nth), failure);
    let committed = match open(Arc::clone(&vfs) as Arc<dyn Vfs>, journal) {
        Ok(mut engine) => run_script(&mut engine, workload).is_ok(),
        Err(_) => false,
    };
    let reached = vfs.failpoints().sites_reached().saturating_sub(base);
    Attempt {
        committed,
        snapshot: vfs.crash(),
        reached,
    }
}

/// Runs the workload to an acknowledged commit, then fails the `n`th call of
/// the checkpoint that follows it.
///
/// **The commit is never the thing that fails here**, which is what makes this
/// campaign's assertion the strong one. The failure is armed after the
/// transaction has been acknowledged, so the only correct answer at every cut
/// point is the committed state - not "the old database or the new one", which
/// is all that can be asked of a crash inside the commit itself.
///
/// @param journal - the journal mode and durability level
/// @param seed - the media model's seed
/// @param nth - which call of the checkpoint to fail
/// @param failure - what to do to it
fn attempt_checkpoint(journal: Journal, seed: u64, nth: u64, failure: Failure) -> Attempt {
    let vfs = built(journal, seed);
    let committed = match open(Arc::clone(&vfs) as Arc<dyn Vfs>, journal) {
        Ok(mut engine) => {
            let wrote = run_script(&mut engine, &WORKLOAD).is_ok();
            // Armed only now, so nothing above this line can fail: every cut
            // point this campaign covers is inside the checkpoint.
            let base = vfs.failpoints().sites_reached();
            vfs.failpoints()
                .fail_nth_call(base.saturating_add(nth), failure);
            let _ = exec(&mut engine, "PRAGMA wal_checkpoint");
            let reached = vfs.failpoints().sites_reached().saturating_sub(base);
            return Attempt {
                committed: wrote,
                snapshot: vfs.crash(),
                reached,
            };
        }
        Err(_) => false,
    };
    Attempt {
        committed,
        snapshot: vfs.crash(),
        reached: 0,
    }
}

/// Runs a campaign over the cut points of a checkpoint.
///
/// Every run's transaction committed before the failure was armed, so the only
/// state recovery may produce is the committed one. A recovery that answers
/// the *old* database is a lost acknowledged commit and fails here, where the
/// commit campaigns have to accept it.
///
/// @param journal - the journal mode and durability level
/// @param failure - what to do to the armed call
/// @param limit - how many cut points to try before giving up
fn checkpointing_campaign(journal: Journal, failure: Failure, limit: u64) -> String {
    let (before, after) = expected_states(journal);
    assert_ne!(before, after, "the workload has to change something");
    let mut report = String::new();
    let mut cut_points = 0u64;
    for nth in 1..=limit {
        let outcome = attempt_checkpoint(journal, 5_100 + nth, nth, failure);
        assert!(
            outcome.committed,
            "call {nth}: the transaction is committed before anything is armed"
        );
        if outcome.reached < nth {
            report.push_str(&format!(
                "{nth}	unarmed	new
"
            ));
            break;
        }
        cut_points = cut_points.saturating_add(1);
        let recovery = recovered(journal, &outcome.snapshot, 7_400 + nth);
        assert_eq!(
            recovery,
            Recovery::Rows(after.clone()),
            "call {nth} of the checkpoint: the commit was acknowledged and then lost"
        );
        report.push_str(&format!(
            "{nth}	checkpoint	new
"
        ));
    }
    assert!(
        cut_points >= 10,
        "a campaign that covers {cut_points} cut points is not a campaign"
    );
    format!(
        "cut points: {cut_points}, every one recovered to the committed state
{report}"
    )
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
fn recovered(journal: Journal, snapshot: &CrashSnapshot, seed: u64) -> Recovery {
    let vfs = Arc::new(SimVfs::recovered(
        SimConfig {
            seed,
            model: MediaModel::default(),
            ..SimConfig::default()
        },
        snapshot,
    ));
    match open(Arc::clone(&vfs) as Arc<dyn Vfs>, journal)
        .and_then(|mut engine| try_contents(&mut engine))
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
fn campaign(journal: Journal, failure: Failure, limit: u64, corruption_allowed: bool) -> String {
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
            report.push_str(&format!("{nth}\tacknowledged\tnew\n"));
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
    let journal = Journal {
        mode: "delete",
        synchronous: "full",
    };
    let report = campaign(journal, Failure::Crash, 220, false);
    record("delete-full-crash.txt", &report);
}

/// TRUNCATE mode has a different commit point - the truncation rather than the
/// deletion - and the same guarantee.
#[test]
fn power_loss_at_every_cut_point_of_a_truncate_commit() {
    let journal = Journal {
        mode: "truncate",
        synchronous: "full",
    };
    let report = campaign(journal, Failure::Crash, 220, false);
    record("truncate-full-crash.txt", &report);
}

/// PERSIST mode's commit point is the header write that makes the journal
/// stop being hot.
#[test]
fn power_loss_at_every_cut_point_of_a_persist_commit() {
    let journal = Journal {
        mode: "persist",
        synchronous: "full",
    };
    let report = campaign(journal, Failure::Crash, 220, false);
    record("persist-full-crash.txt", &report);
}

/// An acknowledged commit survives a power loss at every cut point of the
/// checkpoint that follows it, in each of the three rollback modes.
///
/// **`DELETE` mode's campaigns never touched the rollback journal until this
/// existed, which is why three defects in it survived every run of them.** The
/// journal only holds pre-images while a *checkpoint* is moving pages out of
/// the log and into the data file, and [`WORKLOAD`] on its own commits into
/// the log and stops - so a crash at every one of its cut points crashes in
/// the log and never once in the journal.
///
/// `TRUNCATE` and `PERSIST` were covered by accident. `PRAGMA journal_mode =
/// truncate` is a real change from the connection's default and runs two
/// checkpoints on its way in, so those campaigns crashed inside a checkpoint
/// without anybody intending it, and that is where all three defects were
/// found. `PRAGMA journal_mode = delete` matches the default, returns without
/// doing anything, and left the *default* journal mode the least exercised of
/// the three.
///
/// The assertion here is stronger than the commit campaigns'. Those crash
/// inside the commit and can only ask for the old database or the new one;
/// this arms its failure after the transaction has been acknowledged, so the
/// committed state is the only answer allowed at any cut point. See
/// `crates/inillucent-pool/src/journal.rs` for what it was hiding.
#[test]
fn power_loss_at_every_cut_point_of_a_checkpoint() {
    for mode in ["delete", "truncate", "persist"] {
        let journal = Journal {
            mode,
            synchronous: "full",
        };
        let report = checkpointing_campaign(journal, Failure::Crash, 220);
        record(&format!("{mode}-full-checkpoint-crash.txt"), &report);
    }
}

/// An I/O error and a full disk at every cut point of a checkpoint leave a
/// recoverable database.
///
/// The two failures a checkpoint can be told about, against the same cut
/// points [`power_loss_at_every_cut_point_of_a_checkpoint`] crashes at. A
/// reported failure has to leave the database readable, which is a stronger
/// requirement than the power loss's: the engine was told, so it had the
/// chance to put things back.
#[test]
fn a_reported_failure_at_every_cut_point_of_a_checkpoint_is_recoverable() {
    for failure in [Failure::IoError, Failure::DiskFull] {
        let report = checkpointing_campaign(Journal::DEFAULT, failure, 160);
        let name = match failure {
            Failure::IoError => "delete-full-checkpoint-io-error.txt",
            _ => "delete-full-checkpoint-disk-full.txt",
        };
        record(name, &report);
    }
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
    let journal = Journal::DEFAULT;
    let report = campaign(journal, Failure::ShortWrite, 160, true);
    record("delete-full-short-write.txt", &report);
}

/// A full disk at every cut point leaves a recoverable database.
#[test]
fn a_full_disk_at_every_cut_point_is_recoverable() {
    let journal = Journal::DEFAULT;
    let report = campaign(journal, Failure::DiskFull, 160, false);
    record("delete-full-disk-full.txt", &report);
}

/// An I/O error at every cut point leaves a recoverable database.
#[test]
fn an_io_error_at_every_cut_point_is_recoverable() {
    let journal = Journal::DEFAULT;
    let report = campaign(journal, Failure::IoError, 160, false);
    record("delete-full-io-error.txt", &report);
}

/// A crash *during* recovery leaves either another hot journal that replays to
/// the same result, or the complete old database.
#[test]
fn a_crash_during_recovery_is_idempotent() {
    let journal = Journal::DEFAULT;
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
            let _ = open(Arc::clone(&vfs) as Arc<dyn Vfs>, journal);
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
    let journal = Journal::DEFAULT;
    let vfs = built(journal, 33);
    let mut engine = open(Arc::clone(&vfs) as Arc<dyn Vfs>, journal).expect("it reopens");
    exec(&mut engine, "BEGIN").expect("begins");
    exec(&mut engine, "INSERT INTO t VALUES(10, 'ten', 100)").expect("inserts");
    // The second row of this statement collides with the row the first
    // statement wrote, so ABORT undoes the whole statement - both rows.
    let failed = exec(
        &mut engine,
        "INSERT INTO t VALUES(11, 'eleven', 110), (10, 'again', 120)",
    );
    assert!(failed.is_err(), "the duplicate key must be refused");
    exec(&mut engine, "COMMIT").expect("commits");
    let rows = contents(&mut engine);
    assert!(
        rows.iter().any(|row| row.contains("10")),
        "the first statement's row survived: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("eleven")),
        "the failed statement's earlier row was undone: {rows:?}"
    );
}

/// The single row [`an_open_transactions_row_never_survives_a_checkpoint`]
/// inserts without a `COMMIT` - a transaction a checkpoint must never let
/// reach the file.
const UNCOMMITTED_INSERT: &str = "INSERT INTO t VALUES(99, 'ninety-nine', 990)";

/// As [`attempt_checkpoint`], for a transaction that is never committed:
/// `BEGIN`, one insert, then `PRAGMA wal_checkpoint` with the failure armed at
/// the `n`th call of the checkpoint - and every call from there on is the
/// checkpoint's, because nothing before it may fail either. Unlike
/// `attempt_checkpoint`, what this is checked against afterwards is the
/// schema-only state, because there is no commit for the checkpoint to be
/// allowed to make durable.
///
/// @param journal - the journal mode and durability level
/// @param seed - the media model's seed
/// @param nth - which call of the checkpoint to fail
fn attempt_uncommitted_checkpoint(journal: Journal, seed: u64, nth: u64) -> Attempt {
    let vfs = built(journal, seed);
    let committed = match open(Arc::clone(&vfs) as Arc<dyn Vfs>, journal) {
        Ok(mut engine) => {
            let wrote = exec(&mut engine, "BEGIN")
                .and_then(|()| exec(&mut engine, UNCOMMITTED_INSERT))
                .is_ok();
            // Armed only now, exactly as `attempt_checkpoint` does: every cut
            // point this covers is inside the checkpoint, not the insert.
            let base = vfs.failpoints().sites_reached();
            vfs.failpoints()
                .fail_nth_call(base.saturating_add(nth), Failure::Crash);
            let _ = exec(&mut engine, "PRAGMA wal_checkpoint");
            let reached = vfs.failpoints().sites_reached().saturating_sub(base);
            return Attempt {
                committed: wrote,
                snapshot: vfs.crash(),
                reached,
            };
        }
        Err(_) => false,
    };
    Attempt {
        committed,
        snapshot: vfs.crash(),
        reached: 0,
    }
}

/// An open transaction's row must never survive a checkpoint - whether the
/// checkpoint runs to completion with nothing crashing it, or is interrupted
/// at any point along the way.
///
/// `BEGIN; INSERT INTO t VALUES(99, ...); PRAGMA wal_checkpoint` never
/// commits, so every recovery in this campaign has to answer the schema-only
/// state, not [`WORKLOAD`]'s. Before the fix, `holds_uncommitted` never held
/// anything back - nothing ever moved `uncommitted_lsn` off `u64::MAX` in the
/// shipping engine - so the checkpoint wrote row 99's page straight into the
/// file and `retire_segments_below(durable)` discarded the very log segment
/// holding the insert's own record. Recovery then found the row sitting in
/// the table itself, with nothing left in the log to say it had never
/// committed.
#[test]
fn an_open_transactions_row_never_survives_a_checkpoint() {
    let journal = Journal::NO_ROLLBACK_JOURNAL;
    let schema_only = {
        let vfs = simulator(11_211);
        let mut engine = create(Arc::clone(&vfs) as Arc<dyn Vfs>, journal);
        run_script(&mut engine, &SCHEMA).expect("the schema builds");
        contents(&mut engine)
    };
    let mut cut_points = 0u64;
    for nth in 1..=30u64 {
        let outcome = attempt_uncommitted_checkpoint(journal, 6_600 + nth, nth);
        assert!(
            outcome.committed,
            "call {nth}: the insert has to succeed before the checkpoint is armed"
        );
        cut_points = cut_points.saturating_add(1);
        let recovery = recovered(journal, &outcome.snapshot, 8_800 + nth);
        assert_eq!(
            recovery,
            Recovery::Rows(schema_only.clone()),
            "call {nth} of an uncommitted checkpoint: row 99 survived a transaction \
             that was never committed"
        );
        if outcome.reached < nth {
            break;
        }
    }
    assert!(cut_points > 0, "no cut point of the checkpoint ran");
}

/// How many frames the pool holds for
/// [`a_transaction_the_evictor_writes_back_never_survives_uncommitted`] -
/// deliberately far fewer than the file's usual [`FRAMES`], so an ordinary,
/// uninterrupted transaction dirties more pages than the pool can keep
/// resident and the *evictor* - not a checkpoint - is what reaches
/// `Pool::writeback`.
const SMALL_FRAMES: usize = 64;

/// A transaction wide and long enough to dirty more pages than
/// [`SMALL_FRAMES`] holds, entirely on its own - no `COMMIT` follows it in
/// this file.
const DIRTIES_MANY_PAGES: &str = "INSERT INTO t SELECT 2000 + n, hex(zeroblob(200)), n \
       FROM (WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 300) \
             SELECT n FROM c)";

/// As [`built`], with a pool of [`SMALL_FRAMES`] frames rather than [`FRAMES`].
fn built_small(journal: Journal, seed: u64) -> Arc<SimVfs> {
    let vfs = simulator(seed);
    let mut engine = ImportedDatabase::create_on(
        Arc::clone(&vfs) as Arc<dyn Vfs>,
        path(),
        PAGE_SIZE,
        SMALL_FRAMES,
    )
    .expect("the database is created");
    set_journal(&mut engine, journal.mode, journal.synchronous)
        .expect("journal settings apply on a fresh, unarmed database");
    run_script(&mut engine, &SCHEMA).expect("the schema builds");
    drop(engine);
    vfs
}

/// Runs `BEGIN` and [`DIRTIES_MANY_PAGES`] on a [`SMALL_FRAMES`] pool, with the
/// failure armed at the `n`th call from the transaction's own start - so the
/// crash can land on an ordinary eviction as readily as on the last row
/// inserted, and as readily on nothing at all, if the insert outruns it.
///
/// @param journal - the journal mode and durability level
/// @param seed - the media model's seed
/// @param nth - which call after the transaction begins to fail
fn attempt_evicted_uncommitted(journal: Journal, seed: u64, nth: u64) -> Attempt {
    let vfs = built_small(journal, seed);
    let committed = match ImportedDatabase::open_on(
        Arc::clone(&vfs) as Arc<dyn Vfs>,
        path(),
        PAGE_SIZE,
        SMALL_FRAMES,
    ) {
        Ok(mut engine) => {
            let ready = set_journal(&mut engine, journal.mode, journal.synchronous).is_ok();
            let base = vfs.failpoints().sites_reached();
            vfs.failpoints()
                .fail_nth_call(base.saturating_add(nth), Failure::Crash);
            let wrote = ready
                && exec(&mut engine, "BEGIN")
                    .and_then(|()| exec(&mut engine, DIRTIES_MANY_PAGES))
                    .is_ok();
            // The pool has only [`SMALL_FRAMES`] frames, so most of
            // `DIRTIES_MANY_PAGES`' pages are already written back by
            // ordinary eviction pressure before this line ever runs - this
            // checkpoint is what makes the rest of them durable too, the same
            // way a real process keeps writing after a big insert. Its result
            // is ignored: a crash may interrupt it as readily as the insert,
            // and item 3's refusal is itself part of what this campaign
            // covers.
            let _ = exec(&mut engine, "PRAGMA wal_checkpoint");
            let reached = vfs.failpoints().sites_reached().saturating_sub(base);
            return Attempt {
                committed: wrote,
                snapshot: vfs.crash(),
                reached,
            };
        }
        Err(_) => false,
    };
    Attempt {
        committed,
        snapshot: vfs.crash(),
        reached: 0,
    }
}

/// A row the evictor - not a checkpoint - has already written back for an
/// open transaction must not survive when that transaction crashes before
/// `COMMIT`.
///
/// A pool of [`SMALL_FRAMES`] frames cannot hold every page
/// [`DIRTIES_MANY_PAGES`] dirties, so `Pool::fetch`'s ordinary eviction reaches
/// `writeback` for some of them long before any checkpoint runs - the second
/// way no-steal was missing from the shipping engine, distinct from
/// [`an_open_transactions_row_never_survives_a_checkpoint`]'s checkpoint path.
/// The transaction never commits, so recovery must answer the schema-only
/// state regardless of how far the insert got before the crash - including
/// not at all, when the insert finishes and nothing has crashed it.
#[test]
fn a_transaction_the_evictor_writes_back_never_survives_uncommitted() {
    let journal = Journal::NO_ROLLBACK_JOURNAL;
    let schema_only = {
        let vfs = simulator(33_433);
        let mut engine = ImportedDatabase::create_on(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            path(),
            PAGE_SIZE,
            SMALL_FRAMES,
        )
        .expect("the database is created");
        set_journal(&mut engine, journal.mode, journal.synchronous)
            .expect("journal settings apply on a fresh, unarmed database");
        run_script(&mut engine, &SCHEMA).expect("the schema builds");
        contents(&mut engine)
    };
    let reach = attempt_evicted_uncommitted(journal, 44_499, u64::MAX).reached;
    assert!(reach > 0, "the workload has to reach some injectable calls");
    let mut cut_points = 0u64;
    for nth in 1..=reach {
        let outcome = attempt_evicted_uncommitted(journal, 44_500 + nth, nth);
        cut_points = cut_points.saturating_add(1);
        let recovery = recovered(journal, &outcome.snapshot, 55_600 + nth);
        assert_eq!(
            recovery,
            Recovery::Rows(schema_only.clone()),
            "call {nth}: a row the evictor wrote back for an open transaction survived, \
             though the transaction never committed"
        );
        if outcome.reached < nth {
            break;
        }
    }
    assert!(cut_points > 0, "no cut point of the eviction ran");
}

/// An already-committed row must survive a checkpoint taken while a
/// *different*, later transaction is still open - even though that
/// transaction touches the same page.
///
/// **Pins the defect Fable's review of this ticket found in
/// `ImportedDatabase::checkpoint`.** No-steal correctly holds back a page an
/// open transaction has changed, but the recovery point that
/// `checkpoint`/`checkpoint.rs` used to record was bounded only by
/// `Pool::uncommitted_lsn` - the open transaction's own first record - not by
/// `Pool::oldest_dirty_lsn`, the oldest record *any* held-back page still
/// needs. Those are not the same number when the held-back page's dirty state
/// began with an *earlier*, already-committed write: row 4 is inserted and
/// committed first, moving the page's on-disk copy behind by one record; a
/// second connection then opens a transaction and updates row 1, on the same
/// page, which is what actually makes `holds_uncommitted` true and holds the
/// page back - but the record that page is missing is row 4's insert, not
/// row 1's update, and `uncommitted_lsn` alone names a point *above* it.
/// `PRAGMA user_version` checkpoints unconditionally (`pragma.rs`, unlike
/// `PRAGMA wal_checkpoint`, which refuses with a transaction open) and was
/// the reproduction: without the fix, row 4 - already committed before the
/// second connection ever opened - is gone after a crash that follows.
///
/// **Bounding `recovery_from` by `oldest_dirty_lsn` was necessary but not
/// sufficient - this test kept failing after that fix landed, for a second,
/// independent reason.** `Wal::sequence` reads the segment `roll_segment` just
/// rolled to, and `checkpoint.rs` used to pair `recovery_from` with that
/// segment number unconditionally - which is only correct when `durable`
/// itself is the bound. The moment `oldest_dirty_lsn` pulls `recovery_from`
/// below `durable` - exactly what this test does - the true LSN can sit in an
/// *earlier* segment than the one just rolled to, and pairing it with the new
/// segment's number told the next open's `read_chain` to start reading a
/// segment that does not contain it, silently skipping every record between
/// the two - row 4's insert among them. The fix is `Wal::sequence_containing`,
/// which this test is what caught needing to exist: `checkpoint.rs` now asks
/// it which segment `recovery_from` is actually in, rather than assuming the
/// newest one.
///
/// No fault injection is needed: the defect is in what the recovery point
/// *records* on an uninterrupted, successfully finished checkpoint, not in
/// surviving an interruption of one.
#[test]
fn a_checkpoint_during_a_later_open_transaction_keeps_the_earlier_commit() {
    let journal = Journal::NO_ROLLBACK_JOURNAL;
    let vfs = simulator(70_700);
    {
        let mut engine = create(Arc::clone(&vfs) as Arc<dyn Vfs>, journal);
        run_script(&mut engine, &SCHEMA).expect("the schema builds");
    }
    let expected = {
        let mut engine = open(Arc::clone(&vfs) as Arc<dyn Vfs>, journal).expect("it reopens");
        exec(&mut engine, "INSERT INTO t VALUES(4, 'four', 40)")
            .expect("the autocommit insert commits");
        let rows = contents(&mut engine);
        exec(&mut engine, "BEGIN").expect("the transaction opens");
        exec(&mut engine, "UPDATE t SET c = c + 1 WHERE a = 1")
            .expect("the open transaction's own write runs");
        exec(&mut engine, "PRAGMA user_version = 7").expect("the checkpoint runs");
        rows
    };
    let snapshot = vfs.crash();
    let recovery = recovered(journal, &snapshot, 70_701);
    assert_eq!(
        recovery,
        Recovery::Rows(expected),
        "the row 4 insert, committed before the second transaction ever opened, \
         did not survive a checkpoint taken while that later transaction was still open"
    );
}

/// The highest segment number this file's tests ever create, generously - a
/// bound for [`segments_present`] to check rather than a claim about how many
/// any one run actually makes.
const HIGHEST_PLAUSIBLE_SEGMENT: u64 = 40;

/// Returns which segment numbers up to [`HIGHEST_PLAUSIBLE_SEGMENT`] exist
/// right now.
///
/// **Has to run before the crash, not after.** `SimVfs::access` refuses once
/// `SimVfs::crash` has marked the machine powered off, so a caller that wants
/// to know what existed at the moment of the crash has to ask before making
/// it, then hand the answer to [`force_unsynced_deletes_final`] afterward -
/// asking after crashing silently reports every segment absent regardless of
/// the truth, which was measured directly: it turned this file's own fix into
/// one that failed even with every one of its own defects fixed, because it
/// erased segments the recovered database genuinely still needed.
///
/// @param vfs - the still-live filesystem
fn segments_present(vfs: &SimVfs) -> std::collections::HashSet<u64> {
    (1..=HIGHEST_PLAUSIBLE_SEGMENT)
        .filter(|&sequence| {
            let segment_path = inillucent_wal::writer::segment_path("app.db", None, sequence);
            vfs.access(&segment_path, AccessMode::Exists)
                .unwrap_or(false)
        })
        .collect()
}

/// Forces every WAL segment absent from `present_before_the_crash` to stay
/// absent in the crash's own snapshot.
///
/// **Controls the crash simulator's restoration coin flip rather than
/// sampling it.** `Wal::retire_segments_below` deletes a segment without
/// syncing the directory entry (`Vfs::delete(path, sync_dir: false)`), so
/// `SimVfs::crash` resolves each such pending, unsynced delete independently -
/// a coin flip seeded from the run's own seed
/// (`crash_rng: Rng::new(seed ^ 0xc0ff_ee00)`). A test that crashes right
/// after a retirement and asserts on the outcome is, without this, asserting
/// on a coin flip: the specific seed it happens to run under decides whether
/// the segment a defect needs gone is actually gone in the snapshot handed to
/// recovery. That is exactly the gap that let
/// `a_page_untouched_through_several_checkpoints_never_asks_recovery_for_a_retired_segment`
/// pass with `retained_lsn` removed from `Pool::note_dirty_from` during Codex
/// Sol's review of this ticket - sweeping many seeds was tried first and
/// rejected, because a test that needs luck across a sweep still needs luck.
///
/// This forces the strictest, most conservative outcome every real crash
/// already has to survive: an unsynced directory entry is never assumed
/// durable.
///
/// @param present_before_the_crash - [`segments_present`]'s answer, read
///   before the crash
/// @param snapshot - the crash snapshot to correct in place
fn force_unsynced_deletes_final(
    present_before_the_crash: &std::collections::HashSet<u64>,
    snapshot: &mut CrashSnapshot,
) {
    for sequence in 1..=HIGHEST_PLAUSIBLE_SEGMENT {
        if !present_before_the_crash.contains(&sequence) {
            let segment_path = inillucent_wal::writer::segment_path("app.db", None, sequence);
            snapshot.files.remove(segment_path.as_path());
        }
    }
}

/// A page untouched through several checkpoints must not ask recovery to
/// start below a segment those checkpoints already retired.
///
/// **Pins `Pool::note_dirty_from`'s `retained_lsn` floor - the fix
/// `a_checkpoint_during_a_later_open_transaction_keeps_the_earlier_commit`
/// needed alongside `Wal::sequence_containing`, and a distinct failure mode
/// from either.** A page's `rec_lsn` is only refreshed when the page itself
/// is next modified, not on every checkpoint: table `t`'s page carries the
/// LSN of its own build the whole time nothing touches it, while several
/// *unrelated* checkpoints - on a different table, on different pages - each
/// retire the segments below their own, much higher, recovery point,
/// including the one holding `t`'s original build. `t`'s page is still
/// perfectly correct on disk throughout - nothing has changed about it - but
/// the stamp it carries is now a lie about what the log still holds. The
/// moment it dirties again and a *different*, later transaction forces a
/// checkpoint to hold it back, `oldest_dirty_lsn` would report that stale
/// stamp verbatim and ask recovery to start at a segment that no longer
/// exists.
///
/// See [`force_unsynced_deletes_final`] for why the crash snapshot is
/// corrected before it is read: without that, this assertion depends on the
/// crash simulator's own coin flip for whether the retired segment stays
/// gone, and a single seed is not something to build a regression test on.
///
/// No fault injection is needed beyond the crash itself: the defect is in
/// what an uninterrupted, successfully finished checkpoint records.
#[test]
fn a_page_untouched_through_several_checkpoints_never_asks_recovery_for_a_retired_segment() {
    let journal = Journal::NO_ROLLBACK_JOURNAL;
    let vfs = simulator(70_800);
    {
        let mut engine = create(Arc::clone(&vfs) as Arc<dyn Vfs>, journal);
        run_script(&mut engine, &SCHEMA).expect("the schema builds");
    }
    let expected = {
        let mut engine = open(Arc::clone(&vfs) as Arc<dyn Vfs>, journal).expect("it reopens");
        exec(&mut engine, "CREATE TABLE noise(x INTEGER)").expect("the scratch table builds");
        // Each round writes an unrelated page and checkpoints with nothing
        // open, which retires every segment below that checkpoint's own
        // recovery point - table `t`'s page included, since nothing has
        // touched it since this reopen's own initial checkpoint. A handful of
        // rounds is well past what one retirement needs; it is not tuned to
        // the minimum that works.
        for round in 0..5 {
            exec(&mut engine, &format!("INSERT INTO noise VALUES({round})"))
                .expect("the unrelated insert commits");
            exec(&mut engine, &format!("PRAGMA user_version = {round}"))
                .expect("the unrelated checkpoint runs");
        }
        exec(&mut engine, "INSERT INTO t VALUES(4, 'four', 40)")
            .expect("the autocommit insert commits");
        let rows = contents(&mut engine);
        exec(&mut engine, "BEGIN").expect("the transaction opens");
        exec(&mut engine, "UPDATE t SET c = c + 1 WHERE a = 1")
            .expect("the open transaction's own write runs");
        exec(&mut engine, "PRAGMA user_version = 100").expect("the checkpoint runs");
        rows
    };
    let present_before_the_crash = segments_present(&vfs);
    let mut snapshot = vfs.crash();
    force_unsynced_deletes_final(&present_before_the_crash, &mut snapshot);
    let recovery = recovered(journal, &snapshot, 70_801);
    assert_eq!(
        recovery,
        Recovery::Rows(expected),
        "the row 4 insert, committed before table t's page was dirtied again, did not \
         survive a checkpoint that had to hold that page back - the recovery point \
         pointed at a segment several unrelated checkpoints had already retired"
    );
}
