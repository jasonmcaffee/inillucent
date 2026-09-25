//! Power loss during a commit that spans two databases.
//!
//! Invariant: after any modelled failure the two databases are both what they
//! were before the transaction or both what they would have been after it, and
//! never one of each. That is the whole claim a super-journal makes, and the
//! only way to test it is to cut the commit at every point it has and open both
//! files at each one.
//!
//! **That campaign cannot be run against the new engine yet, and this file now
//! pins the reason instead of running it.** `crates/inillucent-engine/src/attach.rs`'s
//! `ImportedDatabase::attach` builds `Arc::new(OsVfs::new())` for any named
//! file - it never reuses the `Vfs` the caller opened the *main* database on -
//! so a connection built on the fault-injecting simulator cannot `ATTACH` a
//! second file on that same simulator: the attach reaches for the path on the
//! real operating system, where it does not exist, and fails before either
//! database can be exercised. The old engine's `SessionDatabase::open_with`
//! took the `Vfs` explicitly and every attach on that connection used it,
//! which is what let this file's campaign run at all.
//!
//! `attach_does_not_yet_reuse_the_connections_own_vfs` below pins today's
//! refusal. When somebody threads the connection's `Vfs` through `attach()`,
//! that test goes red, and the fix is to restore the campaign this comment
//! describes: build both files on one simulator, number every injectable VFS
//! call across the whole two-file transaction, and reopen both at each cut
//! point in turn - `wal_crash.rs` and `search_crash.rs` are the one-file shape
//! it should match.
//!
//! **What is below does not need that fix, and tests a different claim.** The
//! two-file *commit* campaign above needs one simulator behind both files
//! because it is asking whether a cross-file transaction is decided the same
//! way everywhere; that is still blocked. `an_attached_database_with_an_interrupted_checkpoint_recovers`
//! asks a narrower question - whether a checkpoint interrupted on a file this
//! connection only ever *attaches* is as recoverable as one interrupted on
//! `main` - and a checkpoint tears a file's bytes the same way regardless of
//! which name the connection that ran it used. So that campaign builds and
//! crashes an ordinary single-file database entirely on `SimVfs`, using
//! `durability.rs`'s own `attempt_checkpoint`/`checkpointing_campaign` shape,
//! writes the crash snapshot to the real filesystem, and `ATTACH`es the
//! resulting file - which reaches the real `OsVfs` regardless of today's gap,
//! because the file genuinely is on disk by the time `ATTACH` opens it.

use std::path::PathBuf;
use std::sync::Arc;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_sim::failpoint::Failure;
use inillucent_sim::media::MediaModel;
use inillucent_sim::sim_vfs::{CrashSnapshot, SimConfig, SimVfs};
use inillucent_tree::datum::OwnedDatum;
use inillucent_vfs::Vfs;

/// The page size these runs build at, matching `inillucent_engine::connect::PAGE_SIZE`.
const PAGE_SIZE: usize = 32_768;

/// How many frames the pool holds.
const FRAMES: usize = 4_096;

/// Returns a simulator with the pessimistic device model.
fn simulator(seed: u64) -> Arc<SimVfs> {
    Arc::new(SimVfs::new(SimConfig {
        seed,
        model: MediaModel::default(),
        ..SimConfig::default()
    }))
}

/// The main database's path.
fn path() -> PathBuf {
    PathBuf::from("/sim/main.db")
}

/// Creates the main database on a simulator with nothing written to it yet.
fn create_fresh(vfs: Arc<dyn Vfs>) -> Result<ImportedDatabase, inillucent_base::DbError> {
    ImportedDatabase::create_on(vfs, path(), PAGE_SIZE, FRAMES)
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

/// **Known gap, not yet closed.** `ATTACH` on a connection opened over a
/// custom `Vfs` still opens the attached file through the real OS filesystem,
/// so attaching a second database on the *same* simulator the main one uses
/// fails - it goes looking for `/sim/aux.db` on the real disk, which does not
/// have it.
///
/// See this file's module comment for what closing the gap should restore.
#[test]
fn attach_does_not_yet_reuse_the_connections_own_vfs() {
    let vfs = simulator(1);
    let mut engine =
        create_fresh(Arc::clone(&vfs) as Arc<dyn Vfs>).expect("the main database opens");
    run(&mut engine, "CREATE TABLE t(a INTEGER PRIMARY KEY)").expect("the main schema builds");
    let failure = run(&mut engine, "ATTACH DATABASE '/sim/aux.db' AS aux")
        .expect_err("the attach reaches the real filesystem instead of the simulator");
    let message = format!("{failure}");
    assert!(
        message.contains("unable to open database file"),
        "expected the attach to fail by reaching for a real, nonexistent path; \
         got: {message}"
    );
}

/// The page size and frame count the crash-side database below builds at.
///
/// Deliberately different from `PAGE_SIZE`/`FRAMES` above, which is what the
/// real, never-crashed host database in the same test uses for `main` - a
/// passing campaign then also proves that `attach_file` protects an attached
/// file with a `Journal` sized to *that file's own* page, not the host
/// connection's, which is the second half of what `set_journal_mode` and
/// `attach_file` were changed to do.
const CRASH_PAGE_SIZE: usize = 4_096;
const CRASH_FRAMES: usize = 8_192;

/// The name the crash-side database is known by on its own simulator.
fn crash_name() -> PathBuf {
    PathBuf::from("crashed.db")
}

/// A simulator with the pessimistic device model, for the crash-side database.
fn crash_simulator(seed: u64) -> Arc<SimVfs> {
    Arc::new(SimVfs::new(SimConfig {
        seed,
        model: MediaModel::default(),
        ..SimConfig::default()
    }))
}

/// Runs one statement on the crash-side database.
fn crash_exec(engine: &mut ImportedDatabase, sql: &str) -> Result<(), inillucent_base::DbError> {
    engine.execute_any(sql, &Params::new())?;
    Ok(())
}

/// Runs every statement of a script in order, stopping at the first failure.
fn crash_run(
    engine: &mut ImportedDatabase,
    script: &[&str],
) -> Result<(), inillucent_base::DbError> {
    for statement in script {
        crash_exec(engine, statement)?;
    }
    Ok(())
}

/// The schema the crash-side database builds before anything is armed.
const CRASH_SCHEMA: [&str; 3] = [
    "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER)",
    "CREATE INDEX t_b ON t(b)",
    "INSERT INTO t VALUES(1, 'one', 10), (2, 'two', 20), (3, 'three', 30)",
];

/// The transaction each campaign run commits before its checkpoint is armed.
const CRASH_WORKLOAD: [&str; 5] = [
    "BEGIN",
    "INSERT INTO t VALUES(4, 'four', 40)",
    "UPDATE t SET c = c + 1 WHERE a <= 2",
    "DELETE FROM t WHERE a = 3",
    "COMMIT",
];

/// Forces `delete` mode, the one every journal-mode name maps onto for this
/// campaign's purposes: `journal_for` gives every rollback mode (and, since
/// today's fix, `wal` too) its own `Journal`, so the campaign does not depend
/// on whichever mode a connection now opens in by default.
fn crash_set_journal(engine: &mut ImportedDatabase) -> Result<(), inillucent_base::DbError> {
    crash_exec(engine, "PRAGMA journal_mode = delete")?;
    crash_exec(engine, "PRAGMA synchronous = full")?;
    Ok(())
}

/// Creates the crash-side database, unarmed.
fn crash_create(vfs: Arc<dyn Vfs>) -> ImportedDatabase {
    let mut engine = ImportedDatabase::create_on(vfs, crash_name(), CRASH_PAGE_SIZE, CRASH_FRAMES)
        .expect("the crash-side database is created");
    crash_set_journal(&mut engine).expect("journal settings apply on a fresh, unarmed database");
    engine
}

/// Reopens the crash-side database, unarmed.
fn crash_open(vfs: Arc<dyn Vfs>) -> Result<ImportedDatabase, inillucent_base::DbError> {
    let mut engine = ImportedDatabase::open_on(vfs, crash_name(), CRASH_PAGE_SIZE, CRASH_FRAMES)?;
    crash_set_journal(&mut engine)?;
    Ok(engine)
}

/// Builds the crash-side schema on a fresh simulator and returns it.
fn crash_built(seed: u64) -> Arc<SimVfs> {
    let vfs = crash_simulator(seed);
    let mut engine = crash_create(Arc::clone(&vfs) as Arc<dyn Vfs>);
    crash_run(&mut engine, &CRASH_SCHEMA).expect("the crash-side schema builds");
    drop(engine);
    vfs
}

/// The rows the crash-side workload leaves once committed, from a clean run
/// with nothing armed - the one answer recovering an attachment may give.
fn crash_expected_rows(seed: u64) -> Vec<String> {
    let vfs = crash_built(seed);
    let mut engine = crash_open(Arc::clone(&vfs) as Arc<dyn Vfs>).expect("it reopens");
    crash_run(&mut engine, &CRASH_WORKLOAD).expect("the workload commits");
    crash_contents(&mut engine).expect("the query runs")
}

/// Reads the crash-side table's rows, as text, in a stable order.
fn crash_contents(engine: &mut ImportedDatabase) -> Result<Vec<String>, inillucent_base::DbError> {
    let outcome = engine.execute_any("SELECT a, b, c FROM t ORDER BY a", &Params::new())?;
    Ok(outcome
        .rows
        .iter()
        .map(|row| {
            format!(
                "{:?}|{:?}|{:?}",
                crash_as_integer(row.first()),
                crash_as_text(row.get(1)),
                crash_as_integer(row.get(2)),
            )
        })
        .collect())
}

/// Returns an integer datum as `Option<i64>`.
fn crash_as_integer(value: Option<&OwnedDatum>) -> Option<i64> {
    match value {
        Some(OwnedDatum::Int(value)) => Some(*value),
        _ => None,
    }
}

/// Returns a text datum as `Option<String>`.
fn crash_as_text(value: Option<&OwnedDatum>) -> Option<String> {
    match value {
        Some(OwnedDatum::Text(bytes)) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

/// What one armed run of the crash-side checkpoint did.
struct CrashAttempt {
    /// Whether the workload reported that it committed, before anything was
    /// armed.
    committed: bool,
    /// What the crash-side simulator held after the power loss.
    snapshot: CrashSnapshot,
    /// How many injectable calls the checkpoint reached.
    reached: u64,
}

/// Runs the crash-side workload to an acknowledged commit, then fails the
/// `n`th call of the checkpoint that follows it.
///
/// The same shape as `durability.rs`'s `attempt_checkpoint`: the failure is
/// armed only after the transaction is acknowledged, so the committed state is
/// the only correct answer at every cut point this reaches.
///
/// @param seed - the media model's seed
/// @param nth - which call of the checkpoint to fail
/// @param failure - what to do to it
fn crash_attempt_checkpoint(seed: u64, nth: u64, failure: Failure) -> CrashAttempt {
    let vfs = crash_built(seed);
    let committed = match crash_open(Arc::clone(&vfs) as Arc<dyn Vfs>) {
        Ok(mut engine) => {
            let wrote = crash_run(&mut engine, &CRASH_WORKLOAD).is_ok();
            // Armed only now: every cut point this campaign covers is inside
            // the checkpoint, never inside the commit it follows.
            let base = vfs.failpoints().sites_reached();
            vfs.failpoints()
                .fail_nth_call(base.saturating_add(nth), failure);
            let _ = crash_exec(&mut engine, "PRAGMA wal_checkpoint");
            let reached = vfs.failpoints().sites_reached().saturating_sub(base);
            return CrashAttempt {
                committed: wrote,
                snapshot: vfs.crash(),
                reached,
            };
        }
        Err(_) => false,
    };
    CrashAttempt {
        committed,
        snapshot: vfs.crash(),
        reached: 0,
    }
}

/// Writes every file a crash left behind onto the real filesystem, under
/// `directory`, each at the relative name the simulator knew it by.
///
/// The database, its rollback journal, and its log segments all land beside
/// each other exactly the way a real crash leaves them - `crash_name()`'s
/// bare, slash-free name is what makes `directory.join(path)` compose rather
/// than replace the base, which is what `PathBuf::join` does when the joined
/// path looks absolute.
///
/// @param snapshot - what the simulator reports the media held
/// @param directory - a real, already-created directory to write into
fn materialize(snapshot: &CrashSnapshot, directory: &std::path::Path) -> std::io::Result<()> {
    for (path, bytes) in &snapshot.files {
        let target = directory.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, bytes)?;
    }
    Ok(())
}

/// Returns a scratch directory for one cut point, emptied first.
fn attach_scratch(nth: u64) -> PathBuf {
    let directory = workspace_root()
        .join("_agent_output/task-1926-attach-journal-crash")
        .join(format!("cut-{nth}"));
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::create_dir_all(&directory);
    directory
}

/// What attaching a crashed database, then reading its table, produced.
#[derive(Debug, PartialEq, Eq)]
enum AttachOutcome {
    /// The attachment opened and its table read back these rows.
    Rows(Vec<String>),
    /// `ATTACH`, or the query that followed it, refused - naming the error.
    Refused(String),
}

/// Creates a fresh host database in `directory` and attaches `crashed` to it,
/// under the name `aux`, then reads `aux.t` back.
///
/// The host is a real, never-crashed database on the real `OsVfs`, built at
/// this file's own `PAGE_SIZE`/`FRAMES` - deliberately different from the
/// crash-side database's own, so attaching succeeds only if `attach_file`
/// reads the attached file's *own* page size rather than assuming the host's.
///
/// @param crashed - the crashed database's path, already materialized to disk
/// @param directory - the host's own scratch directory
fn attach_and_read(crashed: &std::path::Path, directory: &std::path::Path) -> AttachOutcome {
    let host_path = directory.join("host.db");
    let mut engine = match ImportedDatabase::create_on(
        Arc::new(inillucent_vfs::OsVfs::new()) as Arc<dyn Vfs>,
        host_path,
        PAGE_SIZE,
        FRAMES,
    ) {
        Ok(engine) => engine,
        Err(error) => return AttachOutcome::Refused(format!("host create: {error}")),
    };
    let attach_sql = format!(
        "ATTACH DATABASE '{}' AS aux",
        crashed.display().to_string().replace('\\', "/")
    );
    if let Err(error) = engine.execute_any(&attach_sql, &Params::new()) {
        return AttachOutcome::Refused(format!("attach: {error}"));
    }
    match engine.execute_any("SELECT a, b, c FROM aux.t ORDER BY a", &Params::new()) {
        Ok(outcome) => AttachOutcome::Rows(
            outcome
                .rows
                .iter()
                .map(|row| {
                    format!(
                        "{:?}|{:?}|{:?}",
                        crash_as_integer(row.first()),
                        crash_as_text(row.get(1)),
                        crash_as_integer(row.get(2)),
                    )
                })
                .collect(),
        ),
        Err(error) => AttachOutcome::Refused(format!("select: {error}")),
    }
}

/// A checkpoint interrupted on a database this connection only ever
/// `ATTACH`es recovers to the committed state, exactly as one interrupted on
/// `main` does.
///
/// **Without `attach_file` calling `replay_hot_journal`, this campaign fails.**
/// `checkpoint_attached` writes an attached file's pages in place the same way
/// `main`'s checkpoint does, so a crash mid-checkpoint tears a page there the
/// same way it used to tear one in `main` before the rollback journal existed.
/// The crash is built entirely on `SimVfs`, using `durability.rs`'s own
/// `attempt_checkpoint` shape - the workload commits, and only then is a
/// failpoint armed inside the `PRAGMA wal_checkpoint` that follows, so the
/// committed state is the only correct answer at every cut point reached. The
/// resulting snapshot is written to the real filesystem and a fresh
/// connection `ATTACH`es it - reaching the real `OsVfs` regardless of
/// `attach_does_not_yet_reuse_the_connections_own_vfs`'s gap above, because by
/// the time `ATTACH` runs, the crashed file genuinely is on disk.
///
/// **One seed per cut point, not one for the whole sweep.** A fixed seed
/// held across every `nth` reliably lands one cut inside
/// `wal_crash.rs`'s already-pinned free-map defect - `Database::checkpoint`
/// writes the free map back with no WAL record behind it, so a crash exactly
/// there is unrecoverable regardless of this ticket's journal, the same
/// "database disk image is malformed" `wal_crash.rs`'s removed cut-29
/// diagnostic named. `durability.rs`'s own `checkpointing_campaign` does not
/// hit it in 220 reached cuts because it reseeds the media model on every
/// attempt, which changes which write the free map's own unprotected window
/// lands on rather than holding it fixed - reseeding here for the same reason
/// is what keeps this campaign measuring the defect it exists for.
#[test]
fn an_attached_database_with_an_interrupted_checkpoint_recovers() {
    // A different seed per cut point, exactly as `durability.rs`'s own
    // `checkpointing_campaign` does, rather than one seed for the whole
    // sweep - see this function's own doc comment for why a fixed seed is
    // the wrong tool here.
    let expected = crash_expected_rows(61_400);
    let limit = 160u64;
    let mut cut_points = 0u64;
    for nth in 1..=limit {
        let attempt = crash_attempt_checkpoint(61_400 + nth, nth, Failure::Crash);
        assert!(
            attempt.committed,
            "call {nth}: the transaction is committed before anything is armed"
        );
        if attempt.reached < nth {
            break;
        }
        cut_points = cut_points.saturating_add(1);
        let directory = attach_scratch(nth);
        let crashed_dir = directory.join("crashed-source");
        std::fs::create_dir_all(&crashed_dir).expect("the crash-side directory is created");
        materialize(&attempt.snapshot, &crashed_dir).expect("the crash snapshot writes to disk");
        let crashed_db = crashed_dir.join(crash_name());
        let outcome = attach_and_read(&crashed_db, &directory);
        assert_eq!(
            outcome,
            AttachOutcome::Rows(expected.clone()),
            "cut {nth} of the attached checkpoint: the commit was acknowledged and then lost"
        );
    }
    assert!(
        cut_points >= 10,
        "a campaign that covers {cut_points} cut points is not a campaign"
    );
}
