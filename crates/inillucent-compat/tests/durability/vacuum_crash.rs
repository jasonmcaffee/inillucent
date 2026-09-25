//! Power loss while `VACUUM` is rewriting the whole file.
//!
//! Invariant: **every state a crash can leave a `VACUUM` in reads back as the
//! database it started from or the database it was making, and never as one
//! that has lost a committed row.** `VACUUM` copies every page of a database
//! into a second file and swaps it in, so it is the one statement whose failure
//! can take the whole database with it.
//!
//! ## This campaign is on the simulator, and it was not
//!
//! Until task-1946's H2 it could not be. `vacuum_in_place` did not go through
//! the connection's VFS: it wrote the rebuilt file with `ImportedDatabase::open`
//! - which constructs a fresh `OsVfs` - swapped it in with `std::fs::rename`,
//! and removed the old log segments with `std::fs::read_dir` and
//! `std::fs::remove_file`. Running the statement on a simulated VFS therefore
//! failed outright with `Open: The system cannot find the path specified`, and
//! the campaign that used to be in this file worked around that by enumerating
//! the *states* a crash could leave rather than the *calls* a run makes -
//! building each one out of `VACUUM INTO`'s output and a hand-written rename.
//!
//! That was an honest workaround for a design it could not change, and it had
//! the weakness every workaround of that shape has: it graded the states
//! somebody had thought of. The rename in particular was never cut, because a
//! `std::fs::rename` is not a thing a test can be inside.
//!
//! `Vfs::rename` exists now and `SimVfs` implements it with a failpoint, so the
//! ordinary campaign harness covers this statement the way it covers every
//! other one: four thousand cuts, each at the Nth VFS call a run makes, with
//! every recovery graded against the two states that are allowed. The cut
//! inside the rename is one of them rather than a case somebody wrote out.
//!
//! `crates/inillucent-engine/src/rebuild.rs` keeps its unit tests against the
//! private primitives, and `vacuum_on_vfs.rs` asserts the other half of H2 -
//! that the connection is still on the file system it was opened on afterwards.

use std::sync::Arc;

use inillucent_compat::crashcampaign::{record, Campaign};
use inillucent_engine::ImportedDatabase;
use inillucent_sim::failpoint::{Failure, Policy, Site};
use inillucent_sim::sim_vfs::{SimConfig, SimVfs};
use inillucent_vfs::Vfs;

/// The database every run starts from, with enough rows that the delete below
/// frees whole pages and the rebuild has real work to do.
const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER);
     CREATE INDEX t_c ON t(c);
     INSERT INTO t(b, c)
     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 400)
     SELECT hex(zeroblob(200)), i * 7 FROM n;";

/// What each run tries to do, and is cut in the middle of.
///
/// **The delete is in the workload rather than in the schema**, and it has to
/// be: the campaign harness refuses a workload that changes nothing, and a
/// `VACUUM` on its own changes no answer - that is the whole point of it. So
/// the run is the delete and the rebuild together, which is also what an
/// application does, and the two states a crash may leave are the database
/// before the delete and the database after the vacuum.
const WORKLOAD: &str = "DELETE FROM t WHERE a > 120;
     VACUUM;";

/// The statements after the workload, so a cut can land past the rebuild.
const TAIL: &str = "SELECT count(*) FROM t; PRAGMA wal_checkpoint;";

/// What a run is graded on.
///
/// The three queries together are the state: the rows, the index's answer, and
/// the totals. Splitting them would let a database that had lost entries from
/// the index match on the other two - and a rebuild is exactly the operation
/// that rewrites every index, so that is the failure worth catching here.
const PROBES: &[&str] = &[
    "SELECT a, length(b), c FROM t ORDER BY a",
    "SELECT a FROM t WHERE c BETWEEN 70 AND 700 ORDER BY c",
    "SELECT count(*), sum(a), sum(c) FROM t",
];

/// Power loss anywhere in a delete and the rebuild that follows it.
#[test]
fn a_vacuum_cut_anywhere_leaves_one_of_the_two_databases() {
    let report = Campaign {
        name: "vacuum-journal-crash",
        mode: "delete",
        schema: SCHEMA,
        workload: WORKLOAD,
        tail: TAIL,
        probes: PROBES,
        failure: Failure::Crash,
        cuts: CUTS,
    }
    .run();
    record("vacuum-journal-crash", &report);
}

/// The same, through a write-ahead log.
#[test]
fn a_vacuum_under_a_log_cut_anywhere_leaves_one_of_the_two_databases() {
    let report = Campaign {
        name: "vacuum-wal-crash",
        mode: "wal",
        schema: SCHEMA,
        workload: WORKLOAD,
        tail: TAIL,
        probes: PROBES,
        failure: Failure::Crash,
        cuts: CUTS,
    }
    .run();
    record("vacuum-wal-crash", &report);
}

/// A device that refuses a write never leaves the database a mixture.
#[test]
fn a_reported_write_failure_during_a_vacuum_leaves_one_of_the_two() {
    let report = Campaign {
        name: "vacuum-journal-io",
        mode: "delete",
        schema: SCHEMA,
        workload: WORKLOAD,
        tail: TAIL,
        probes: PROBES,
        failure: Failure::IoError,
        cuts: CUTS,
    }
    .run();
    record("vacuum-journal-io", &report);
}

/// How many cut points each campaign grades.
///
/// **A hundred and fifty rather than the four thousand its siblings use, and
/// the difference is what a run costs.** `overflow_crash` cuts a transaction;
/// each of these cuts a transaction *and* a whole-database rebuild, so one cut
/// here is worth far more wall time than one of those. Measured on this
/// machine: four thousand ran for over half an hour and was killed, three
/// hundred took 647 seconds for the three campaigns together, and a hundred and
/// fifty takes about half of that - which puts this suite alongside
/// `overflow_crash` at 298 seconds rather than doubling the strict pass.
///
/// It is not a number that can be lowered until the campaign stops covering
/// anything: the harness refuses fewer than twenty cuts and asserts that both
/// legitimate states were reached, so a count too small to get past the delete
/// and into the rebuild fails rather than passing quietly.
const CUTS: u64 = 150;

/// The page size the direct case below runs at.
const PAGE_SIZE: usize = 32_768;

/// How many frames its pool holds.
const FRAMES: usize = 64;

/// The path the direct case's database takes inside the simulator.
const DIRECT_PATH: &str = "/sim/vacuum_rename.db";

/// Returns a simulator with the campaign's device model.
///
/// @param seed - what the device model's randomness starts from
fn simulator(seed: u64) -> Arc<SimVfs> {
    Arc::new(SimVfs::new(SimConfig {
        seed,
        ..SimConfig::default()
    }))
}

/// Runs a script, reporting rather than panicking.
///
/// @param engine - the open connection
/// @param script - the statements, separated by semicolons
fn script(engine: &mut ImportedDatabase, script: &str) -> Result<(), inillucent_base::DbError> {
    for statement in script.split(';') {
        if statement.trim().is_empty() {
            continue;
        }
        engine.execute_any(statement, &inillucent_exec::physical::Params::new())?;
    }
    Ok(())
}

/// Returns the rows a database answers the probes with.
///
/// @param engine - the open connection
fn state(engine: &mut ImportedDatabase) -> Result<Vec<String>, inillucent_base::DbError> {
    let mut rows = Vec::new();
    for probe in PROBES {
        rows.push(format!("-- {probe}"));
        let outcome = engine.execute_any(probe, &inillucent_exec::physical::Params::new())?;
        for row in outcome.rows {
            rows.push(format!("{row:?}"));
        }
    }
    Ok(rows)
}

/// The cut the rest of this file could not reach until the rename went through
/// the VFS: a crash *inside* `Vfs::rename`, with the rebuilt file written and
/// the original still in place.
///
/// **The one crash-sensitive moment of the whole statement.** Everything before
/// it is written beside the database and can be thrown away; everything after
/// it is bookkeeping over a file that already holds the rebuilt bytes. The
/// campaigns above reach this point among four thousand others; this asks it
/// directly, so a change that stopped the rename from being a failpoint at all
/// would fail a test that names the rename rather than quietly reducing the
/// coverage of one that counts cuts.
#[test]
fn a_crash_inside_the_rename_recovers_the_original() {
    let vfs = simulator(51_515);
    let path = std::path::PathBuf::from(DIRECT_PATH);

    let expected = {
        let mut engine = ImportedDatabase::create_on(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            path.clone(),
            PAGE_SIZE,
            FRAMES,
        )
        .expect("the database is created in the simulator");
        script(&mut engine, "PRAGMA journal_mode=delete").expect("the journal mode applies");
        script(&mut engine, SCHEMA).expect("the schema builds");
        script(&mut engine, "DELETE FROM t WHERE a > 120").expect("the delete runs");
        state(&mut engine).expect("the prepared database reads")
    };

    // Every rename this run makes loses power. There is exactly one: the swap
    // at the end of `vacuum_in_place`.
    vfs.failpoints()
        .set(Site::Rename, Policy::Always(Failure::Crash));
    let mut engine = ImportedDatabase::open_on(
        Arc::clone(&vfs) as Arc<dyn Vfs>,
        path.clone(),
        PAGE_SIZE,
        FRAMES,
    )
    .expect("the database reopens");
    script(&mut engine, "PRAGMA journal_mode=delete").expect("the journal mode applies");
    let refused = script(&mut engine, "VACUUM");
    assert!(
        refused.is_err(),
        "the VACUUM reported success through a machine that lost power inside its rename"
    );
    assert_eq!(
        vfs.failpoints()
            .counts()
            .get(&Site::Rename)
            .copied()
            .unwrap_or(0),
        1,
        "the rename was never reached, so this test cut nothing"
    );

    let snapshot = vfs.crash();
    drop(engine);

    let recovered_vfs = Arc::new(SimVfs::recovered(
        SimConfig {
            seed: 52_525,
            ..SimConfig::default()
        },
        &snapshot,
    ));
    let mut recovered =
        ImportedDatabase::open_on(recovered_vfs as Arc<dyn Vfs>, path, PAGE_SIZE, FRAMES)
            .expect("the original still opens after a crash inside the rename");
    assert_eq!(
        state(&mut recovered).expect("the recovered database reads"),
        expected,
        "a crash inside the rename must leave the database exactly as it was"
    );
}
