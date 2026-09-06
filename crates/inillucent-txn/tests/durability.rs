//! Crash campaigns through the whole engine: a database file, a log, and the
//! transactions over both.
//!
//! Invariant: **the oracle is built before the crash and from the other side of
//! the interface.** A campaign asks the engine to commit, and
//! records which commits *returned* - then crashes, reopens, and compares what
//! the reopened engine holds against that list. Nothing here asks the engine
//! whether it recovered correctly.
//!
//! `inillucent-wal`'s own campaigns crash a log with no data file behind it.
//! These crash the pair, which is where the write-ahead rule can actually be
//! violated: a checkpoint that flushed a page the log had not described would
//! pass every test in that crate and fail here.

use std::sync::Arc;

use inillucent_pool::page::{self, header};
use inillucent_pool::{Options, PageId};
use inillucent_sim::{Failure, Policy, SimConfig, SimVfs, Site};
use inillucent_txn::engine::{Begin, Engine, EngineOptions};
use inillucent_txn::redo::RefuseRows;
use inillucent_vfs::{DbPath, Vfs};
use inillucent_wal::record::Body;
use inillucent_wal::writer::WalOptions;
use inillucent_wal::Synchronous;

/// The page size the campaigns use.
const PAGE: usize = 512;

/// Returns engine options at the test page size.
///
/// @param policy - the sync policy
fn options(policy: Synchronous) -> EngineOptions {
    EngineOptions {
        database: Options::default().with_page_size(PAGE).with_frames(16),
        wal: WalOptions {
            synchronous: policy,
            segment_bytes: 32_768,
        },
        busy_timeout_ms: 0,
    }
}

/// Builds the page image one transaction writes.
///
/// Two fields in two places, so that a half-applied transaction is visible: a
/// workload whose transactions each wrote one thing cannot tell a torn
/// transaction from a missing one.
///
/// @param round - the transaction number
fn image_for(round: u64) -> Vec<u8> {
    let mut image = vec![0u8; PAGE];
    page::write_common(&mut image, page::PageKind::Leaf, 0, 1).expect("a header");
    if let Some(slot) = image.get_mut(64..72) {
        slot.copy_from_slice(&round.to_le_bytes());
    }
    if let Some(slot) = image.get_mut(256..264) {
        slot.copy_from_slice(&(round.wrapping_mul(7)).to_le_bytes());
    }
    image
}

/// Reads the two fields back out of a page, or `None` when it is not there.
///
/// @param engine - the engine
/// @param page - the page
fn fields_of(engine: &Engine, page: PageId) -> Option<(u64, u64)> {
    engine.with_pool(|pool| {
        let guard = pool.fetch(page).ok()?;
        let first = page::read_u64(&guard, 64).ok()?;
        let second = page::read_u64(&guard, 256).ok()?;
        Some((first, second))
    })
}

/// Runs the workload and returns the transactions the engine acknowledged.
///
/// Each transaction allocates a page, writes it **into the pool**, and logs both
/// - which is the shape a real write path has and the reason this is stated
/// rather than assumed. The first version of this workload only *logged* the
/// page write and never performed it, so the pages existed nowhere but the log;
/// a checkpoint then moved the recovery start past them and the data was gone.
/// That looked like a checkpointing bug and was a workload that did half of what
/// a transaction does. A campaign whose workload is not a workload measures
/// nothing, which is the failure this project keeps finding in its instruments.
///
/// The page is stamped with the LSN of the record that describes it, which is
/// what makes the write-ahead rule and the page-LSN rule both apply to it.
///
/// @param engine - the engine
/// @param count - how many transactions
/// @param checkpoint_every - take a checkpoint this often, or zero for never
fn workload(engine: &Engine, count: u64, checkpoint_every: u64) -> Vec<u64> {
    let mut acknowledged = Vec::new();
    for round in 1..=count {
        let Ok(mut txn) = engine.begin(Begin::Immediate) else {
            break;
        };
        let page = PageId(8 + round);
        let mut image = image_for(round);
        let Ok(lsn) = txn.log(Body::WritePage {
            page: page.0,
            image: &image,
        }) else {
            break;
        };
        if txn.log(Body::AllocPage { page: page.0 }).is_err() {
            break;
        }
        if page::write_u64(&mut image, header::LSN, lsn).is_err() {
            break;
        }
        let placed = engine.with_database(|database| {
            database.claim(page)?;
            database.install(page, &image)
        });
        if placed.is_err() {
            break;
        }
        match txn.commit() {
            Ok(_) => acknowledged.push(round),
            Err(_) => break,
        }
        drop(txn);
        if checkpoint_every > 0 && round % checkpoint_every == 0 && engine.checkpoint().is_err() {
            break;
        }
    }
    acknowledged
}

/// Holds durability, atomicity and the prefix property on a reopened engine.
///
/// @param engine - the reopened engine
/// @param acknowledged - the commits the workload was told had succeeded
/// @param context - what to say when it fails
fn assert_recovered(engine: &Engine, acknowledged: &[u64], context: &str) {
    let mut visible = Vec::new();
    for round in 1..=64u64 {
        if let Some((first, second)) = fields_of(engine, PageId(8 + round)) {
            if first == 0 && second == 0 {
                continue;
            }
            assert_eq!(
                (first, second),
                (round, round.wrapping_mul(7)),
                "{context}: page {} is half written",
                8 + round
            );
            visible.push(round);
        }
    }
    for round in acknowledged {
        assert!(
            visible.contains(round),
            "{context}: transaction {round} was acknowledged and is gone \
             (recovered {:?})",
            engine.recovered()
        );
    }
    let highest = visible.iter().copied().max().unwrap_or(0);
    for round in 1..=highest {
        assert!(
            visible.contains(&round),
            "{context}: transaction {round} is missing from a run that recovered up to \
             {highest}, so the replayed set is not a prefix of the log"
        );
    }
}

/// Crashing at every write leaves exactly the acknowledged prefix.
#[test]
fn crashing_at_every_write_leaves_the_acknowledged_prefix() {
    let mut arms = 0usize;
    let mut interrupted = 0usize;
    for nth in 1..=48u64 {
        let vfs = Arc::new(SimVfs::new(SimConfig::default()));
        let path = DbPath::new("crash-write.rdb");
        let acknowledged = {
            let Ok(engine) = Engine::create(
                Arc::clone(&vfs) as Arc<dyn Vfs>,
                &path,
                options(Synchronous::Full),
            ) else {
                continue;
            };
            vfs.failpoints()
                .set(Site::Write, Policy::Nth(nth, Failure::Crash));
            workload(&engine, 12, 0)
        };
        let snapshot = vfs.crash();
        arms += 1;
        if acknowledged.len() < 12 {
            interrupted += 1;
        }

        let recovered_vfs: Arc<dyn Vfs> =
            Arc::new(SimVfs::recovered(SimConfig::default(), &snapshot));
        let Ok(engine) = Engine::open(recovered_vfs, &path, options(Synchronous::Full), RefuseRows)
        else {
            // A crash during the very first writes can leave a file with no
            // readable meta page at all, which is a database that never
            // existed rather than one that lost data. There is nothing to
            // check, and saying so is better than pretending the arm passed.
            assert!(
                acknowledged.is_empty(),
                "crash at write {nth}: {} commits were acknowledged and the database \
                 will not reopen",
                acknowledged.len()
            );
            continue;
        };
        assert_recovered(&engine, &acknowledged, &format!("crash at write {nth}"));
    }
    assert!(arms >= 30, "the campaign only ran {arms} arms");
    assert!(
        interrupted > 0,
        "no arm actually interrupted the workload, so the campaign proved nothing"
    );
}

/// Crashing at every sync leaves exactly the acknowledged prefix.
///
/// The sync is the commit point under `FULL`, so this is the campaign that kills
/// a mutant which acknowledges a commit before its sync returns.
#[test]
fn crashing_at_every_sync_leaves_the_acknowledged_prefix() {
    let mut arms = 0usize;
    for nth in 1..=32u64 {
        let vfs = Arc::new(SimVfs::new(SimConfig::default()));
        let path = DbPath::new("crash-sync.rdb");
        let acknowledged = {
            let Ok(engine) = Engine::create(
                Arc::clone(&vfs) as Arc<dyn Vfs>,
                &path,
                options(Synchronous::Full),
            ) else {
                continue;
            };
            vfs.failpoints()
                .set(Site::Sync, Policy::Nth(nth, Failure::Crash));
            workload(&engine, 12, 0)
        };
        let snapshot = vfs.crash();
        arms += 1;
        let recovered_vfs: Arc<dyn Vfs> =
            Arc::new(SimVfs::recovered(SimConfig::default(), &snapshot));
        let Ok(engine) = Engine::open(recovered_vfs, &path, options(Synchronous::Full), RefuseRows)
        else {
            assert!(
                acknowledged.is_empty(),
                "crash at sync {nth}: {} commits were acknowledged and the database \
                 will not reopen",
                acknowledged.len()
            );
            continue;
        };
        assert_recovered(&engine, &acknowledged, &format!("crash at sync {nth}"));
    }
    assert!(arms >= 20, "the campaign only ran {arms} arms");
}

/// Crashing while checkpointing leaves the acknowledged prefix.
///
/// A checkpoint moves the recovery start forward and writes pages, which is the
/// one operation that can lose a committed change by *succeeding* in the wrong
/// order. Crashing through it is the only way to find out.
#[test]
fn crashing_during_a_checkpoint_leaves_the_acknowledged_prefix() {
    let mut arms = 0usize;
    let mut checkpointed = 0usize;
    for nth in 1..=64u64 {
        let vfs = Arc::new(SimVfs::new(SimConfig::default()));
        let path = DbPath::new("crash-checkpoint.rdb");
        let acknowledged = {
            let Ok(engine) = Engine::create(
                Arc::clone(&vfs) as Arc<dyn Vfs>,
                &path,
                options(Synchronous::Full),
            ) else {
                continue;
            };
            vfs.failpoints()
                .set(Site::Write, Policy::Nth(nth, Failure::Crash));
            let acknowledged = workload(&engine, 12, 3);
            checkpointed += usize::from(engine.stats().checkpoints > 0);
            acknowledged
        };
        let snapshot = vfs.crash();
        arms += 1;
        let recovered_vfs: Arc<dyn Vfs> =
            Arc::new(SimVfs::recovered(SimConfig::default(), &snapshot));
        let Ok(engine) = Engine::open(recovered_vfs, &path, options(Synchronous::Full), RefuseRows)
        else {
            assert!(
                acknowledged.is_empty(),
                "crash at write {nth} with checkpoints: {} commits were acknowledged \
                 and the database will not reopen",
                acknowledged.len()
            );
            continue;
        };
        assert_recovered(
            &engine,
            &acknowledged,
            &format!("crash at write {nth} with checkpoints every three commits"),
        );
    }
    assert!(arms >= 40, "the campaign only ran {arms} arms");
    assert!(
        checkpointed > 0,
        "no arm ever reached a checkpoint, so the campaign is the previous one again"
    );
}

/// Failing the Nth call is reported and never leaves a torn transaction.
#[test]
fn failing_the_nth_call_is_reported_and_leaves_no_torn_transaction() {
    for failure in [Failure::DiskFull, Failure::IoError] {
        for nth in 1..=20u64 {
            let vfs = Arc::new(SimVfs::new(SimConfig::default()));
            let path = DbPath::new("nth.rdb");
            let acknowledged = {
                let Ok(engine) = Engine::create(
                    Arc::clone(&vfs) as Arc<dyn Vfs>,
                    &path,
                    options(Synchronous::Full),
                ) else {
                    continue;
                };
                vfs.failpoints().fail_nth_call(nth, failure);
                let acknowledged = workload(&engine, 12, 0);
                vfs.failpoints().set(Site::Write, Policy::Off);
                vfs.failpoints().set(Site::Sync, Policy::Off);
                acknowledged
            };
            let Ok(engine) = Engine::open(
                Arc::clone(&vfs) as Arc<dyn Vfs>,
                &path,
                options(Synchronous::Full),
                RefuseRows,
            ) else {
                assert!(
                    acknowledged.is_empty(),
                    "{failure:?} at call {nth}: {} commits were acknowledged and the \
                     database will not reopen",
                    acknowledged.len()
                );
                continue;
            };
            assert_recovered(
                &engine,
                &acknowledged,
                &format!("{failure:?} at call {nth}"),
            );
        }
    }
}

/// A checkpoint under a failing file system does not advance the meta page.
///
/// The failure this catches has no error at the time: a checkpoint that wrote
/// its meta record before its pages, and then failed, would leave a meta page
/// naming a state the file does not hold. The assertion is that the reopened
/// engine still holds everything acknowledged, which is what a meta page that
/// ran ahead would break.
#[test]
fn a_failed_checkpoint_does_not_lose_a_committed_change() {
    for nth in 1..=24u64 {
        let vfs = Arc::new(SimVfs::new(SimConfig::default()));
        let path = DbPath::new("checkpoint-fail.rdb");
        let acknowledged = {
            let Ok(engine) = Engine::create(
                Arc::clone(&vfs) as Arc<dyn Vfs>,
                &path,
                options(Synchronous::Full),
            ) else {
                continue;
            };
            let acknowledged = workload(&engine, 6, 0);
            vfs.failpoints()
                .set(Site::Write, Policy::Nth(nth, Failure::IoError));
            let _ = engine.checkpoint();
            vfs.failpoints().set(Site::Write, Policy::Off);
            acknowledged
        };
        let Ok(engine) = Engine::open(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            &path,
            options(Synchronous::Full),
            RefuseRows,
        ) else {
            continue;
        };
        assert_recovered(
            &engine,
            &acknowledged,
            &format!("a checkpoint that failed at write {nth}"),
        );
    }
}

/// A database recovered, checkpointed and recovered again is the same database.
///
/// Idempotence at the engine level rather than at the log's: the log's own test
/// recovers twice into one store, and this one puts a checkpoint between the two
/// recoveries, which moves the recovery start and rewrites the meta page. A
/// checkpoint that recorded the wrong start would show up here and nowhere else.
#[test]
fn recovering_checkpointing_and_recovering_again_is_the_same_database() {
    let vfs: Arc<dyn Vfs> = Arc::new(inillucent_vfs::MemoryVfs::new());
    let path = DbPath::new("idempotent.rdb");
    let acknowledged = {
        let engine =
            Engine::create(Arc::clone(&vfs), &path, options(Synchronous::Full)).expect("an engine");
        workload(&engine, 10, 0)
    };

    let first: Vec<Option<(u64, u64)>> = {
        let engine = Engine::open(
            Arc::clone(&vfs),
            &path,
            options(Synchronous::Full),
            RefuseRows,
        )
        .expect("the first reopen");
        assert_recovered(&engine, &acknowledged, "the first reopen");
        engine.checkpoint().expect("a checkpoint");
        (1..=10u64)
            .map(|round| fields_of(&engine, PageId(8 + round)))
            .collect()
    };

    let second: Vec<Option<(u64, u64)>> = {
        let engine = Engine::open(
            Arc::clone(&vfs),
            &path,
            options(Synchronous::Full),
            RefuseRows,
        )
        .expect("the second reopen");
        assert_recovered(&engine, &acknowledged, "the second reopen");
        // The only record above the checkpoint's own start is the `Checkpoint`
        // marker the first reopen wrote. Every page image is below it and in
        // the file, which is what the checkpoint was for; a second reopen that
        // replayed one would mean the checkpoint recorded a start it had not
        // actually reached.
        let outcome = engine.recovered();
        assert_eq!(
            (outcome.scanned, outcome.applied),
            (1, 1),
            "the second reopen replayed records the checkpoint had already put in the file"
        );
        assert!(outcome.last_checkpoint.is_some());
        (1..=10u64)
            .map(|round| fields_of(&engine, PageId(8 + round)))
            .collect()
    };

    assert_eq!(first, second, "the two reopens disagree about the database");
}

/// A log belonging to another database is not applied to this one.
#[test]
fn a_foreign_log_is_not_applied() {
    let vfs: Arc<dyn Vfs> = Arc::new(inillucent_vfs::MemoryVfs::new());
    let path = DbPath::new("foreign.rdb");
    {
        let engine =
            Engine::create(Arc::clone(&vfs), &path, options(Synchronous::Full)).expect("an engine");
        workload(&engine, 4, 0);
    }
    // A fresh database at the same path has a fresh uuid, and the segments
    // beside it belong to the one that was there before.
    let replaced = Engine::create(Arc::clone(&vfs), &path, options(Synchronous::Full))
        .expect("a fresh database");
    drop(replaced);
    let engine = Engine::open(
        Arc::clone(&vfs),
        &path,
        options(Synchronous::Full),
        RefuseRows,
    )
    .expect("the fresh database opens");
    assert_eq!(
        engine.recovered().applied,
        0,
        "another database's log was applied"
    );
    assert_eq!(fields_of(&engine, PageId(9)), None);
}

/// Every `synchronous` policy survives a crash the way it promises to.
///
/// `FULL` loses nothing acknowledged. `NORMAL` may lose recent commits and may
/// not tear one. Both are asserted, because a `NORMAL` that never lost anything
/// would be `FULL` wearing a different name, and a `NORMAL` that tore a
/// transaction would be broken rather than fast.
#[test]
fn each_policy_survives_a_crash_the_way_it_promises() {
    let mut normal_losses = 0usize;
    for nth in 1..=32u64 {
        for policy in [Synchronous::Full, Synchronous::Normal] {
            let vfs = Arc::new(SimVfs::new(SimConfig::default()));
            let path = DbPath::new("policy.rdb");
            let acknowledged = {
                let Ok(engine) =
                    Engine::create(Arc::clone(&vfs) as Arc<dyn Vfs>, &path, options(policy))
                else {
                    continue;
                };
                vfs.failpoints()
                    .set(Site::Write, Policy::Nth(nth, Failure::Crash));
                workload(&engine, 10, 0)
            };
            let snapshot = vfs.crash();
            let recovered_vfs: Arc<dyn Vfs> =
                Arc::new(SimVfs::recovered(SimConfig::default(), &snapshot));
            let Ok(engine) = Engine::open(recovered_vfs, &path, options(policy), RefuseRows) else {
                continue;
            };
            match policy {
                Synchronous::Full => {
                    assert_recovered(&engine, &acknowledged, &format!("FULL, crash at {nth}"))
                }
                _ => {
                    // Atomicity and the prefix property hold under NORMAL too;
                    // only durability of the last commits is given up.
                    assert_recovered(&engine, &[], &format!("NORMAL, crash at {nth}"));
                    let present: Vec<u64> = acknowledged
                        .iter()
                        .copied()
                        .filter(|round| fields_of(&engine, PageId(8 + round)).is_some())
                        .collect();
                    normal_losses += acknowledged.len().saturating_sub(present.len());
                }
            }
        }
    }
    assert!(
        normal_losses > 0,
        "NORMAL lost nothing across the whole campaign, so the policy is doing nothing"
    );
}

/// The write-ahead rule holds across a checkpoint under a log that is behind.
///
/// A page carrying an LSN above the durable log must not reach the file, and a
/// checkpoint is the caller most likely to try: it flushes everything dirty. The
/// engine's checkpoint syncs the log first, so this constructs the situation
/// directly - a page stamped past the log - and asserts the flush is refused.
#[test]
fn a_checkpoint_cannot_write_a_page_the_log_has_not_described() {
    let vfs: Arc<dyn Vfs> = Arc::new(inillucent_vfs::MemoryVfs::new());
    let path = DbPath::new("write-ahead.rdb");
    let engine =
        Engine::create(Arc::clone(&vfs), &path, options(Synchronous::Full)).expect("an engine");
    workload(&engine, 3, 0);
    engine.wal().sync().expect("a sync");

    let ahead = engine.wal().durable_end() + 4_096;
    let mut image = image_for(99);
    page::write_u64(&mut image, header::LSN, ahead).expect("an lsn");
    engine.with_database(|database| database.install(PageId(40), &image).expect("installed"));

    let refused = engine
        .with_pool(|pool| pool.flush())
        .expect_err("a page ahead of the log must not be written");
    assert!(refused
        .detail()
        .unwrap_or_default()
        .contains("ahead of the log"));
    assert_eq!(
        fields_of(&engine, PageId(40)),
        Some((99, 99 * 7)),
        "the page is still in the pool; it is the *file* it must not reach"
    );
}
