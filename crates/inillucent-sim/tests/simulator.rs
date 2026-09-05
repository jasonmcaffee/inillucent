//! The simulator's own acceptance evidence.
//!
//! Invariant: the simulator behaves like a disk where a disk is specified, and
//! misbehaves only where it was told to. These tests hold it to both halves.

use std::sync::Arc;

use inillucent_sim::failpoint::{Failure, Policy, Site};
use inillucent_sim::media::MediaModel;
use inillucent_sim::schedule::{ActorId, Decisions, Scheduler};
use inillucent_sim::sim_vfs::{set_current_actor, SimConfig, SimVfs};
use inillucent_vfs::conformance;
use inillucent_vfs::contract::{FileLock, OpenOptions, SyncMode, Vfs};
use inillucent_vfs::path::DbPath;

/// Returns a simulator configured for a run.
fn simulator(seed: u64) -> SimVfs {
    SimVfs::new(SimConfig {
        seed,
        ..SimConfig::default()
    })
}

/// Writes a report next to the other evidence artifacts when asked to.
fn record(name: &str, body: &str) {
    let Ok(root) = std::env::var("INILLUCENT_TEST_ARTIFACTS") else {
        return;
    };
    let directory = std::path::PathBuf::from(root);
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let _ = std::fs::write(directory.join(name), body);
}

/// The simulator must satisfy the same contract as a disk, or crash evidence
/// gathered on it says nothing about the database anyone actually runs.
#[test]
fn the_simulator_passes_the_vfs_conformance_suite() {
    let vfs = simulator(1782);
    let report = conformance::run(&vfs, &DbPath::from("/sim"));
    record("conformance-simulator.txt", &report.to_text());
    assert!(report.is_clean(), "{}", report.to_text());
    assert!(report.passed() >= 24, "{}", report.to_text());
}

/// Runs a fixed workload and returns the trace digest it produced.
fn workload(seed: u64) -> (u32, String) {
    let vfs = simulator(seed);
    let path = DbPath::from("/sim/app.db");
    let file = vfs
        .open(&path, OpenOptions::main_db())
        .expect("the database opens");
    for page in 0..8u64 {
        let payload = vec![(page as u8).wrapping_mul(17); 512];
        file.write_all_at(page * 512, &payload)
            .expect("the write lands");
    }
    file.sync(SyncMode::Full).expect("the sync succeeds");
    let mut buffer = [0u8; 512];
    for page in 0..8u64 {
        file.read_exact_at(page * 512, &mut buffer)
            .expect("the read succeeds");
    }
    let mut noise = [0u8; 16];
    vfs.randomness(&mut noise).expect("randomness is available");
    (vfs.trace().digest(), vfs.trace().to_jsonl())
}

/// The same seed must produce a byte-identical trace, which is the property the
/// whole replay story rests on.
#[test]
fn the_same_seed_replays_an_identical_trace() {
    let (first_digest, first_text) = workload(4242);
    let (second_digest, second_text) = workload(4242);
    assert_eq!(first_digest, second_digest);
    assert_eq!(first_text, second_text);
    record("simulator-trace.jsonl", &first_text);
}

/// The seed has to reach the parts of a run that are allowed to vary. The
/// operation trace deliberately does not depend on it - the same workload does
/// the same operations - but the randomness the VFS hands out must.
#[test]
fn the_seed_drives_the_simulated_randomness() {
    let sample = |seed: u64| {
        let vfs = simulator(seed);
        let mut bytes = [0u8; 32];
        vfs.randomness(&mut bytes).expect("randomness is available");
        bytes
    };
    assert_eq!(sample(7), sample(7));
    assert_ne!(sample(7), sample(8));
}

/// Runs two actors against one simulated file under a scheduler, and returns
/// the trace digest together with the schedule that produced it.
fn concurrent_run(decisions: Decisions) -> (u32, Vec<usize>, String) {
    let vfs = Arc::new(simulator(9001));
    let scheduler = Scheduler::new(2, decisions);
    vfs.attach_scheduler(Arc::clone(&scheduler));
    let path = DbPath::from("/sim/contended.db");
    let mut handles = Vec::new();
    for id in 0..2u32 {
        let vfs = Arc::clone(&vfs);
        let scheduler = Arc::clone(&scheduler);
        let path = path.clone();
        handles.push(std::thread::spawn(move || {
            let actor = ActorId(id);
            set_current_actor(actor);
            let file = vfs
                .open(&path, OpenOptions::main_db())
                .expect("the database opens");
            for round in 0..4u64 {
                let _ = file.lock(FileLock::Shared);
                let payload = vec![id as u8; 64];
                let _ = file.write_all_at(round * 64 + u64::from(id) * 256, &payload);
                let _ = file.sync(SyncMode::Normal);
                let _ = file.unlock(FileLock::None);
            }
            drop(file);
            scheduler.finish(actor);
        }));
    }
    for handle in handles {
        let _ = handle.join();
    }
    (
        vfs.trace().digest(),
        scheduler.recorded_schedule(),
        vfs.trace().to_jsonl(),
    )
}

/// A recorded schedule must reproduce the run it came from, event for event.
/// Without this, a concurrency failure found by the explorer could not be
/// reproduced, and a bug that cannot be reproduced cannot be fixed.
#[test]
fn a_recorded_schedule_replays_an_identical_event_trace() {
    let (digest, schedule, text) = concurrent_run(Decisions::Seeded(31337));
    record("simulator-schedule.txt", &format!("{schedule:?}\n"));
    record("simulator-concurrent-trace.jsonl", &text);
    for _ in 0..4 {
        let (replay_digest, _, replay_text) =
            concurrent_run(Decisions::Replay(schedule.clone(), 0));
        assert_eq!(replay_digest, digest, "replaying the schedule diverged");
        assert_eq!(replay_text, text);
    }
}

/// A write that was synced must survive a power loss, at every seed. This is
/// the single promise every later durability claim is built on.
#[test]
fn a_synced_write_survives_every_simulated_power_loss() {
    for seed in 0..200 {
        let vfs = simulator(seed);
        let path = DbPath::from("/sim/durable.db");
        let file = vfs
            .open(&path, OpenOptions::main_db())
            .expect("the database opens");
        file.write_all_at(0, &[0xa5; 4096])
            .expect("the write lands");
        file.sync(SyncMode::Full).expect("the sync succeeds");
        file.write_all_at(4096, &[0x5a; 4096])
            .expect("the second write lands");
        drop(file);
        let snapshot = vfs.crash();

        let recovered = SimVfs::recovered(
            SimConfig {
                seed,
                ..SimConfig::default()
            },
            &snapshot,
        );
        let reopened = recovered
            .open(&path, OpenOptions::main_db())
            .expect("the database reopens");
        let mut buffer = [0u8; 4096];
        reopened
            .read_exact_at(0, &mut buffer)
            .expect("the synced page is readable");
        assert_eq!(buffer, [0xa5; 4096], "seed {seed} lost a synced page");
    }
}

/// The crash must sometimes lose the unsynced page, or the model is not
/// modelling anything and the durability tests would pass vacuously.
#[test]
fn an_unsynced_write_is_sometimes_lost_across_seeds() {
    let mut lost = 0;
    for seed in 0..200 {
        let vfs = simulator(seed);
        let path = DbPath::from("/sim/volatile.db");
        let file = vfs
            .open(&path, OpenOptions::main_db())
            .expect("the database opens");
        file.write_all_at(0, &[0x11; 512]).expect("the write lands");
        file.sync(SyncMode::Full).expect("the sync succeeds");
        file.write_all_at(0, &[0x22; 512])
            .expect("the second write lands");
        drop(file);
        let snapshot = vfs.crash();
        let bytes = snapshot.files.get(std::path::Path::new("/sim/volatile.db"));
        if bytes.is_some_and(|bytes| bytes.first() != Some(&0x22)) {
            lost += 1;
        }
    }
    assert!(lost > 0, "no seed ever lost an unsynced write");
}

/// The systematic campaign must terminate, and every injected failure must be
/// reported as an error rather than as a panic or a silent success.
#[test]
fn the_failpoint_campaign_terminates_with_every_failure_reported() {
    let mut deepest = 0;
    for nth in 1..=64u64 {
        let vfs = simulator(77);
        vfs.failpoints().fail_nth_call(nth, Failure::IoError);
        let path = DbPath::from("/sim/injected.db");
        let outcome = (|| -> Result<(), String> {
            let file = vfs
                .open(&path, OpenOptions::main_db())
                .map_err(|error| error.detail().to_string())?;
            file.write_all_at(0, &[0x7f; 1024])
                .map_err(|error| error.detail().to_string())?;
            file.sync(SyncMode::Full)
                .map_err(|error| error.detail().to_string())?;
            let mut buffer = [0u8; 1024];
            file.read_exact_at(0, &mut buffer)
                .map_err(|error| error.detail().to_string())?;
            Ok(())
        })();
        deepest = deepest.max(vfs.failpoints().sites_reached());
        if vfs.failpoints().sites_reached() < nth {
            assert!(
                outcome.is_ok(),
                "a run that never reached the failpoint still failed"
            );
            assert!(nth > 1, "the campaign ended before it injected anything");
            return;
        }
        assert!(
            outcome.is_err(),
            "call {nth} was injected but the run reported success"
        );
    }
    panic!("the campaign never ran out of failpoints; deepest was {deepest}");
}

/// A short write stores part of the data and reports success, which is the
/// failure the pager has to survive by checksumming rather than by trusting.
#[test]
fn a_short_write_reports_success_and_stores_half() {
    let vfs = simulator(5);
    vfs.failpoints()
        .set(Site::Write, Policy::Nth(1, Failure::ShortWrite));
    let path = DbPath::from("/sim/short.db");
    let file = vfs
        .open(&path, OpenOptions::main_db())
        .expect("the database opens");
    file.write_all_at(0, &[0xcc; 512])
        .expect("a short write reports success");
    let bytes = vfs.visible_bytes(&path).expect("the file exists");
    assert_eq!(bytes.len(), 256, "a short write stored half of the payload");
    assert!(
        bytes.iter().all(|byte| *byte == 0xcc),
        "the stored half is wrong"
    );
}

/// A crash snapshot must be writable as an artifact, because that is how a
/// failing run hands the image to whoever debugs it.
#[test]
fn crash_artifacts_are_written_to_disk() {
    let vfs = simulator(11);
    let path = DbPath::from("/sim/artifact.db");
    let file = vfs
        .open(&path, OpenOptions::main_db())
        .expect("the database opens");
    file.write_all_at(0, &[0x99; 2048])
        .expect("the write lands");
    file.sync(SyncMode::Full).expect("the sync succeeds");
    file.write_all_at(2048, &[0x88; 2048])
        .expect("the second write lands");
    drop(file);
    let snapshot = vfs.crash();

    let mut directory = std::env::temp_dir();
    directory.push(format!("inillucent-sim-artifacts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    snapshot
        .write_artifacts(&directory)
        .expect("the artifact directory is writable");
    let manifest = std::fs::read_to_string(directory.join("crash-manifest.txt"))
        .expect("the manifest was written");
    assert!(manifest.contains("artifact.db"), "{manifest}");
    assert!(directory.join("file-000.bin").exists());
    let _ = std::fs::remove_dir_all(&directory);
}

/// A device that promises whole-sector atomicity must never tear, and one that
/// does not must sometimes tear. The model has to follow its own declaration or
/// the durability matrix is measuring the wrong device.
#[test]
fn the_media_model_follows_its_declaration() {
    let atomic = SimVfs::new(SimConfig {
        seed: 3,
        model: MediaModel {
            atomic_write_size: 4096,
            powersafe_overwrite: true,
            ..MediaModel::default()
        },
        ..SimConfig::default()
    });
    let path = DbPath::from("/sim/atomic.db");
    let file = atomic
        .open(&path, OpenOptions::main_db())
        .expect("the database opens");
    file.write_all_at(0, &[0x01; 512]).expect("the write lands");
    file.sync(SyncMode::Full).expect("the sync succeeds");
    file.write_all_at(0, &[0x02; 512])
        .expect("the second write lands");
    drop(file);
    let snapshot = atomic.crash();
    let bytes = snapshot
        .files
        .get(std::path::Path::new("/sim/atomic.db"))
        .expect("the file is in the snapshot");
    let all_old = bytes.iter().take(512).all(|byte| *byte == 0x01);
    let all_new = bytes.iter().take(512).all(|byte| *byte == 0x02);
    assert!(
        all_old || all_new,
        "an atomic device produced a torn sector"
    );
}

/// A disk-full failure, a permission failure and an interrupt must each arrive
/// as the code SQLite uses for it, on the operation that hit it. The pager
/// above will branch on these, so a failure that arrives as a generic I/O error
/// is a failure the pager cannot handle correctly.
#[test]
fn injected_failures_arrive_as_the_right_code() {
    use inillucent_base::error::PrimaryCode;

    let cases = [
        (Site::Write, Failure::DiskFull, PrimaryCode::Full),
        (Site::Write, Failure::Permission, PrimaryCode::Perm),
        (Site::Write, Failure::Interrupt, PrimaryCode::Interrupt),
        (Site::Write, Failure::IoError, PrimaryCode::IoErr),
    ];
    for (site, failure, expected) in cases {
        let vfs = simulator(21);
        vfs.failpoints().set(site, Policy::Nth(1, failure));
        let path = DbPath::from("/sim/codes.db");
        let file = vfs
            .open(&path, OpenOptions::main_db())
            .expect("the database opens");
        let error = file
            .write_all_at(0, &[0u8; 512])
            .expect_err("the injected failure must be reported");
        assert_eq!(error.code(), expected, "{failure:?} arrived as {error:?}");
        assert!(
            error.detail().contains("injected"),
            "an injected failure should say so: {error:?}"
        );
    }
}

/// A short read must zero-fill and report SQLITE_IOERR_SHORT_READ whether it
/// came from a truncated file or from injection, because the pager cannot tell
/// the two apart and must handle both the same way.
#[test]
fn an_injected_short_read_matches_a_truncated_one() {
    let injected = simulator(22);
    injected
        .failpoints()
        .set(Site::Read, Policy::Nth(1, Failure::ShortRead));
    let path = DbPath::from("/sim/short-read.db");
    let file = injected
        .open(&path, OpenOptions::main_db())
        .expect("the database opens");
    file.write_all_at(0, &[0xee; 4096])
        .expect("the write lands");
    let mut buffer = [0x11u8; 4096];
    let error = file
        .read_exact_at(0, &mut buffer)
        .expect_err("the injected short read must be reported");
    assert_eq!(
        error.extended(),
        inillucent_base::error::ExtendedCode::IO_ERR_SHORT_READ
    );
    assert!(
        buffer.iter().all(|byte| *byte == 0),
        "the buffer was not zeroed"
    );

    let truncated = simulator(23);
    let file = truncated
        .open(&path, OpenOptions::main_db())
        .expect("the database opens");
    file.write_all_at(0, &[0xee; 512]).expect("the write lands");
    let natural = file
        .read_exact_at(0, &mut buffer)
        .expect_err("reading past the end must be reported");
    assert_eq!(natural.extended(), error.extended());
}
