//! Two faults that are not a power loss, and what the engine does with them.
//!
//! Invariant: **a fault that is not a power loss is detected or has no effect,
//! and is never served as a different answer.** That is a weaker claim than
//! `durability.rs`'s - a crash leaves the database in one of two states, and
//! these two faults leave it in neither - and it is the only claim that can be
//! made about damage the engine did not cause and cannot undo. What it forbids
//! is the one outcome that matters: a query that answers, and answers wrongly.
//!
//! ## Why these two and not the ones already modelled
//!
//! `durability.rs` injects a crash at every cut point, `faults.rs` injects an
//! allocation failure, and `corruption.rs` flips bytes. All three change bytes
//! or stop the machine, so a checksum or a replay sees them. These two do
//! neither (task-2066 section 4.4.8):
//!
//! - **A misdirected write** lands whole, at the wrong offset, and reports
//!   success. Both pages read back as well formed pages and pass their own
//!   checksum; what is wrong is which one is where. inillucent's page header
//!   carries an LSN, a checksum, a kind, a level, the tree and the right
//!   sibling, and **no page number** - so nothing in the page itself can say it
//!   is in the wrong place, and detection has to come from the tree above it.
//! - **A sync that lies** returns success and leaves the write cache
//!   unresolved. Every durability argument in this engine rests on `sync`
//!   meaning what it says, so the question is not whether data is lost - it is
//!   - but whether the next open notices.
//!
//! Both are what consumer drives and container file systems actually do.
//!
//! ## What a case asserts
//!
//! Each arm builds a database, injects one fault, reopens, and compares what
//! comes back against what the workload committed. Three outcomes are
//! acceptable and one is not: the open refuses, the integrity check refuses, or
//! the rows are exactly right. A different set of rows is the failure, and it
//! is the only thing these cases are looking for.

use std::path::PathBuf;
use std::sync::Arc;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_sim::failpoint::Failure;
use inillucent_sim::media::MediaModel;
use inillucent_sim::sim_vfs::{CrashSnapshot, SimConfig, SimVfs};
use inillucent_tree::datum::OwnedDatum;
use inillucent_vfs::Vfs;

/// The page size and pool these cases build at.
///
/// Small pages so the workload spans several of them: a misdirected write that
/// lands inside the one page a whole table fits in would be overwriting the
/// page with itself.
const PAGE_SIZE: usize = 4_096;
const FRAMES: usize = 512;

/// The schema and rows every case commits.
const WORKLOAD: [&str; 4] = [
    "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)",
    "CREATE INDEX t_b ON t(b)",
    "BEGIN",
    "COMMIT",
];

/// How many rows the workload writes.
///
/// Enough to fill several leaves at this page size, so a page landing one page
/// along is a page the tree is actually using.
const ROWS: i64 = 400;

/// Returns the path every case uses.
fn path() -> PathBuf {
    PathBuf::from("shapes.db")
}

/// Runs one statement through the engine's own write path.
///
/// @param engine - the database
/// @param sql - the statement
fn exec(engine: &mut ImportedDatabase, sql: &str) -> Result<(), inillucent_base::DbError> {
    engine.execute_any(sql, &Params::new())?;
    Ok(())
}

/// Builds the fixture on one file system and commits it.
///
/// @param vfs - the file system to build on
fn build(vfs: Arc<dyn Vfs>) -> Result<(), inillucent_base::DbError> {
    let mut engine = ImportedDatabase::create_on(vfs, path(), PAGE_SIZE, FRAMES)?;
    exec(&mut engine, WORKLOAD[0])?;
    exec(&mut engine, WORKLOAD[1])?;
    exec(&mut engine, WORKLOAD[2])?;
    for row in 0..ROWS {
        exec(
            &mut engine,
            &format!("INSERT INTO t VALUES({row}, 'row {row:04}')"),
        )?;
    }
    exec(&mut engine, WORKLOAD[3])?;
    engine.checkpoint()?;
    Ok(())
}

/// What one reopen of a damaged database produced.
enum Reopened {
    /// The open or the integrity check refused, which is the honest answer.
    Refused,
    /// It opened, checked out and answered; the rows it answered with.
    Answered(Vec<Vec<OwnedDatum>>),
}

/// Reopens a snapshot, checks it, and reads the table back.
///
/// **The integrity check runs before the query**, because a file the checker
/// refuses is a file that has been detected, and asking it a question
/// afterwards would be asking a question of something already known to be
/// wrong.
///
/// @param snapshot - the file system as the fault left it
fn reopen(snapshot: &CrashSnapshot) -> Reopened {
    let vfs = Arc::new(SimVfs::recovered(SimConfig::default(), snapshot));
    let Ok(mut engine) = ImportedDatabase::open_on(vfs as Arc<dyn Vfs>, path(), PAGE_SIZE, FRAMES)
    else {
        return Reopened::Refused;
    };
    if engine.check_trees().is_err() {
        return Reopened::Refused;
    }
    match engine.execute_any("SELECT a, b FROM t ORDER BY a", &Params::new()) {
        Ok(outcome) => Reopened::Answered(outcome.rows),
        Err(_) => Reopened::Refused,
    }
}

/// Returns the rows the workload committed, which is what a clean reopen owes.
fn expected() -> Vec<Vec<OwnedDatum>> {
    (0..ROWS)
        .map(|row| {
            vec![
                OwnedDatum::Int(row),
                OwnedDatum::Text(format!("row {row:04}").into_bytes()),
            ]
        })
        .collect()
}

/// Returns a simulator with the pessimistic device model.
///
/// @param seed - the run's seed
fn simulator(seed: u64) -> Arc<SimVfs> {
    Arc::new(SimVfs::new(SimConfig {
        seed,
        model: MediaModel::default(),
        ..SimConfig::default()
    }))
}

/// **A misdirected write is refused or has no effect, and is never answered
/// around.**
///
/// The fault is injected at every write of the workload in turn, which is what
/// makes this a campaign rather than one arrangement that happened to be
/// caught. Each arm reopens and compares; a reopen that answers a *different*
/// set of rows is the failure this exists to find, because that is a caller
/// being told something untrue by a file that passed every check it has.
///
/// The count at the end is the second half. A campaign where every arm refused
/// to build would assert nothing and report green, so the number of arms that
/// reached a reopen is asserted too.
#[test]
fn a_misdirected_write_is_refused_or_has_no_effect() {
    let reach = {
        let vfs = simulator(4_408);
        let _ = build(Arc::clone(&vfs) as Arc<dyn Vfs>);
        vfs.failpoints().sites_reached()
    };
    assert!(
        reach > 20,
        "the workload reached {reach} injectable calls, which is too few to be a campaign"
    );

    let wanted = expected();
    let mut reopened = 0usize;
    let mut wrong: Vec<String> = Vec::new();
    // Every eleventh call, because the fault is expensive to arrange and the
    // shape does not change between adjacent writes of the same page.
    for nth in (1..=reach).step_by(11) {
        let vfs = simulator(4_408);
        let base = vfs.failpoints().sites_reached();
        vfs.failpoints()
            .fail_nth_call(base.saturating_add(nth), Failure::Misdirected);
        if build(Arc::clone(&vfs) as Arc<dyn Vfs>).is_err() {
            continue;
        }
        let snapshot = vfs.crash();
        match reopen(&snapshot) {
            Reopened::Refused => reopened = reopened.saturating_add(1),
            Reopened::Answered(rows) => {
                reopened = reopened.saturating_add(1);
                if rows != wanted {
                    wrong.push(format!(
                        "cut {nth}: the reopen answered {} rows and the workload committed {}",
                        rows.len(),
                        wanted.len()
                    ));
                }
            }
        }
    }
    assert!(
        reopened > 0,
        "no arm of the campaign reached a reopen, so nothing was measured"
    );
    assert!(
        wrong.is_empty(),
        "a misdirected write was answered around rather than refused:\n  {}",
        wrong.join("\n  ")
    );
}

/// **A damaged page read through the pool is refused, and the rate is
/// recorded.**
///
/// This is the claim `phase2_campaigns::corrupt_pages_never_panic` cannot make.
/// That case hands bytes to `LeafRef::parse` and `InteriorRef::parse` directly,
/// below the layer that checks a page's crc32c - so a decoder there has nothing
/// to check its input against, and answering a plausible value for plausible
/// bytes is what it is for. It measured 587 damaged pages that answered
/// something the undamaged page did not (task-2066 section 4.4.6).
///
/// Here the same damage goes through the pool, which verifies the checksum on
/// the way in from the file, so the claim is the strong one: refused, or
/// exactly the rows the workload committed.
///
/// **The rates are recorded here rather than in a file the run rewrites**, for
/// the reason `crash_reports.rs` gives about `tests/crash/`: a number the run
/// produces is what the last run produced, and the last run is what is under
/// suspicion. A number in this source is what a person accepted.
#[test]
fn a_damaged_page_read_through_the_pool_is_refused() {
    let vfs = simulator(4_410);
    build(Arc::clone(&vfs) as Arc<dyn Vfs>).expect("the fixture builds");
    let clean = vfs.crash();
    let whole = clean
        .files
        .get(&path())
        .cloned()
        .expect("the database is in the snapshot");
    assert!(
        whole.len() > PAGE_SIZE * 4,
        "the fixture is {} bytes, which is too few pages to damage",
        whole.len()
    );
    let wanted = expected();

    // Page two onward: pages zero and one are the meta pages, which check their
    // own checksum on every decode and are `corrupt_pages_never_panic`'s to
    // measure.
    let pages = whole.len() / PAGE_SIZE;
    let mut sweep = inillucent_compat::damage::Sweep::default();
    let mut served: Vec<String> = Vec::new();
    for page in 2..pages {
        for offset in [0usize, 37, PAGE_SIZE / 2, PAGE_SIZE - 1] {
            let at = page.saturating_mul(PAGE_SIZE).saturating_add(offset);
            let mut damaged = clean.clone();
            let Some(bytes) = damaged.files.get_mut(&path()) else {
                continue;
            };
            let Some(byte) = bytes.get_mut(at) else {
                continue;
            };
            *byte = byte.wrapping_add(0x5A);
            match reopen(&damaged) {
                Reopened::Refused => sweep.refuse(),
                Reopened::Answered(rows) if rows == wanted => sweep.unchanged(),
                Reopened::Answered(rows) => {
                    sweep.total = sweep.total.saturating_add(1);
                    served.push(format!(
                        "page {page} byte {offset}: {} rows came back where {} were committed",
                        rows.len(),
                        wanted.len()
                    ));
                }
            }
        }
    }
    println!(
        "damaged pages through the pool: {} of {} refused ({:.3}), {} unchanged",
        sweep.refused,
        sweep.total,
        sweep.detection_rate(),
        sweep.unchanged
    );
    assert!(
        sweep.total >= 32,
        "only {} damaged pages were read, which is too few to be a sweep",
        sweep.total
    );
    assert!(
        served.is_empty(),
        "a damaged page was served as a different answer rather than refused:
  {}",
        served
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<String>>()
            .join(
                "
  "
            )
    );
    // Measured 2026-09-22. The floor is what a person accepted; raising it is
    // an edit somebody makes on purpose, and a fall is a detector that stopped
    // detecting.
    assert!(
        sweep.detection_rate() >= 0.50,
        "the detection rate fell to {:.3} and its recorded floor is 0.500",
        sweep.detection_rate()
    );
}

/// **A truncated database is refused, and that rate is asserted near one.**
///
/// Truncation is the one damage shape with no ambiguity: every page past the
/// cut reads as zeroes through the pool's short-read fill, and zeroes fail the
/// page checksum. A rate below one here would mean a truncated file being read
/// as a shorter database, which is data loss served as an answer.
#[test]
fn a_truncated_database_is_refused() {
    let vfs = simulator(4_411);
    build(Arc::clone(&vfs) as Arc<dyn Vfs>).expect("the fixture builds");
    let clean = vfs.crash();
    let whole = clean
        .files
        .get(&path())
        .cloned()
        .expect("the database is in the snapshot");
    let pages = whole.len() / PAGE_SIZE;
    assert!(
        pages > 4,
        "the fixture holds {pages} pages, which is too few"
    );

    let wanted = expected();
    let mut sweep = inillucent_compat::damage::Sweep::default();
    let mut served: Vec<String> = Vec::new();
    // Every page boundary from the third onward: cutting inside the meta pages
    // is a file that is not a database, which the open refuses for a different
    // reason and which says nothing about truncation.
    for page in 2..pages {
        let mut damaged = clean.clone();
        let Some(bytes) = damaged.files.get_mut(&path()) else {
            continue;
        };
        bytes.truncate(page.saturating_mul(PAGE_SIZE));
        match reopen(&damaged) {
            Reopened::Refused => sweep.refuse(),
            Reopened::Answered(rows) if rows == wanted => sweep.unchanged(),
            Reopened::Answered(rows) => {
                sweep.total = sweep.total.saturating_add(1);
                served.push(format!(
                    "cut at page {page}: {} rows came back where {} were committed",
                    rows.len(),
                    wanted.len()
                ));
            }
        }
    }
    println!(
        "truncated databases: {} of {} refused ({:.3}), {} unchanged",
        sweep.refused,
        sweep.total,
        sweep.detection_rate(),
        sweep.unchanged
    );
    assert!(
        served.is_empty(),
        "a truncated database was served as a different answer:
  {}",
        served.join(
            "
  "
        )
    );
    assert!(
        sweep.total >= 3,
        "only {} cuts were read, which is too few to be a sweep",
        sweep.total
    );
    assert!(
        sweep.detection_rate() >= 0.95,
        "truncation was detected {:.3} of the time, and it has to be near one",
        sweep.detection_rate()
    );
}

/// **A sync that lies loses data, and the loss is refused rather than served.**
///
/// The device acknowledges every flush and makes nothing durable, so the file a
/// power loss leaves behind holds whatever the pessimistic model resolves the
/// cached sectors to - dropped, torn or garbage. What is asserted is not that
/// the rows survive, because they cannot: it is that the reopen does not answer
/// a *different* set of rows.
///
/// The control arm is the same workload on a device that does not lie. Without
/// it, a case that built nothing at all would pass.
#[test]
fn a_sync_that_lies_is_refused_rather_than_answered_around() {
    let honest = Arc::new(SimVfs::new(SimConfig {
        seed: 4_409,
        model: MediaModel::default(),
        ..SimConfig::default()
    }));
    build(Arc::clone(&honest) as Arc<dyn Vfs>).expect("the fixture builds on an honest device");
    let control = reopen(&honest.crash());
    match control {
        Reopened::Answered(rows) => assert_eq!(
            rows,
            expected(),
            "an honest device did not keep what was committed, so the arm below proves nothing"
        ),
        Reopened::Refused => {
            panic!("an honest device produced a database its own checker refuses")
        }
    }

    let wanted = expected();
    let mut answered = 0usize;
    let mut wrong: Vec<String> = Vec::new();
    for seed in 0..6u64 {
        let lying = Arc::new(SimVfs::new(SimConfig {
            seed,
            model: MediaModel {
                sync_is_a_lie: true,
                ..MediaModel::default()
            },
            ..SimConfig::default()
        }));
        if build(Arc::clone(&lying) as Arc<dyn Vfs>).is_err() {
            continue;
        }
        match reopen(&lying.crash()) {
            Reopened::Refused => {}
            Reopened::Answered(rows) => {
                answered = answered.saturating_add(1);
                if rows != wanted {
                    wrong.push(format!(
                        "seed {seed}: the reopen answered {} rows and the workload committed {}",
                        rows.len(),
                        wanted.len()
                    ));
                }
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "a device whose sync lies served a different answer rather than refusing:\n  {}",
        wrong.join("\n  ")
    );
    let _ = answered;
}
