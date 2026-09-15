//! A page the log holds whole, torn, and read by a record that is not the one
//! that would have fixed it.
//!
//! Invariant: **a page the log describes comes back; a page it does not is
//! refused by name.** Those are the two halves of what recovery promises, and
//! the second is what stops the first from being a licence to guess.
//!
//! ## What was wrong
//!
//! Redo reads the page a logical row record changes - an `INSERT` into a leaf
//! reads the leaf, adds the row and writes it back. So a crash that tore a page
//! failed the replay at the **first** record naming that page, even when a
//! later record in the same window carried the page whole. The window's end
//! state was knowable and recovery refused the file anyway.
//!
//! `crates/inillucent-engine/src/recovery.rs` answers it where the damage is
//! reported: when the logical pass fails with a corruption code, every record
//! in the window that carries a whole page image is applied - they need no
//! catalog and no row decoder - and the same pass runs again against a file the
//! log has made whole. Re-running is sound because redo is idempotent on the
//! page-LSN rule: a record the first attempt applied has stamped its pages with
//! its own LSN, so the second attempt skips it.
//!
//! **On the failure and not before it.** `wal_crash`'s commit campaign measured
//! what happens when the images go in unconditionally: `read_checkpointed_catalog`
//! then succeeds where it used to fail, which flips the `repaired` flag and
//! seeds the logical pass with the checkpoint-time catalog rather than the
//! end-of-window one - and the one cut of twenty-three that reaches the new
//! state stopped reaching it. A committed transaction lost is a worse defect
//! than the one being fixed.
//!
//! ## Why this file rather than a crash sweep
//!
//! `free_map_checkpoint_crash.rs`'s sweep cannot ask the question: a cut either
//! tears a page the log describes or one it does not, and only the first is
//! recoverable, so a refusal there is ambiguous. Both cases here are built
//! deliberately, and the page is chosen by reading the log rather than by being
//! named, so neither can go stale when the layout moves.

use std::path::PathBuf;
use std::sync::Arc;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_sim::media::MediaModel;
use inillucent_sim::sim_vfs::{CrashSnapshot, SimConfig, SimVfs};
use inillucent_tree::datum::OwnedDatum;
use inillucent_vfs::path::DbPath;
use inillucent_vfs::{AccessMode, Vfs};
use inillucent_wal::record::{Body, Record};
use inillucent_wal::recover::{recover, RecoveryStart, Redo};

/// The page size this fixture builds at, and the frames the pool holds.
///
/// Large enough that nothing is evicted and no checkpoint happens on its own,
/// so the window under test is the only one in play.
const PAGE_SIZE: usize = 4_096;
const FRAMES: usize = 8_192;

/// How many rows the splitting fixture ends with, and how wide each body is:
/// enough that the table's leaves split inside the window under test, which is
/// what puts a whole page image there after the records that filled it.
const SPLITTING_ROWS: i64 = 200;
const SPLITTING_WIDTH: usize = 300;

/// The same fixture without the splits, so the window holds records that read
/// pages and no record that carries one whole.
const NARROW_ROWS: i64 = 20;
const NARROW_WIDTH: usize = 8;

/// Returns the path every run in this file uses.
fn path() -> PathBuf {
    PathBuf::from("torn-page-with-image.rdb")
}

/// Runs one statement, failing the test with what the engine said.
///
/// @param engine - the database
/// @param sql - the statement
fn run(engine: &mut ImportedDatabase, sql: &str) {
    engine
        .execute_any(sql, &Params::new())
        .unwrap_or_else(|error| {
            panic!(
                "{sql}: {} ({})",
                error.message(),
                error.detail().unwrap_or_default()
            )
        });
}

/// Returns the ids the table holds, in key order.
///
/// @param engine - the database
fn ids(engine: &mut ImportedDatabase) -> Vec<i64> {
    let outcome = engine
        .execute_any("SELECT id FROM doc ORDER BY id", &Params::new())
        .expect("the table reads");
    outcome
        .rows
        .iter()
        .filter_map(|row| match row.first() {
            Some(OwnedDatum::Int(id)) => Some(*id),
            _ => None,
        })
        .collect()
}

/// What the log says about one page.
#[derive(Default)]
struct PageStory {
    /// The LSN of the highest record carrying this page whole, when there is
    /// one.
    image: Option<u64>,
    /// The LSN of the highest record that reads this page to change it.
    logical: Option<u64>,
}

/// Reads the log and says, per page, what kinds of record name it.
#[derive(Default)]
struct WhatTheLogSays {
    /// One story per page the log names.
    pages: std::collections::BTreeMap<u64, PageStory>,
}

impl Redo for WhatTheLogSays {
    /// Always "not yet applied": this observer rebuilds nothing, so every
    /// record's pages are worth handing to `redo`.
    ///
    /// @param _page - unused
    fn page_lsn(&mut self, _page: u64) -> inillucent_base::DbResult<Option<u64>> {
        Ok(None)
    }

    /// Files each record under the pages it names, by whether it carries an
    /// image of them or reads them.
    ///
    /// @param record - the record to inspect
    /// @param _wanted - unused; see `page_lsn`
    fn redo(&mut self, record: &Record<'_>, _wanted: &[bool]) -> inillucent_base::DbResult<()> {
        let lsn = record.lsn;
        let mut image = |page: u64| {
            self.pages.entry(page).or_default().image = Some(lsn);
        };
        match record.body {
            Body::WritePage { page, .. } => image(page),
            Body::CompactLeaf {
                page, image: [], ..
            } => {
                self.pages.entry(page).or_default().logical = Some(lsn);
            }
            Body::CompactLeaf { page, .. } => image(page),
            Body::Structural {
                left,
                right,
                parent,
                ..
            } => {
                image(left);
                image(right);
                image(parent);
            }
            Body::InsertRow { page, .. }
            | Body::DeleteRow { page, .. }
            | Body::UpdateInPlace { page, .. } => {
                self.pages.entry(page).or_default().logical = Some(lsn);
            }
            _ => {}
        }
        Ok(())
    }
}

/// Returns the lowest segment number that still has a file behind it.
///
/// Neither end of the range is safe to assume: an earlier checkpoint may have
/// retired segment 1, and the current sequence may hold nothing yet.
///
/// @param vfs - the file system the log lives on
/// @param wal - the log
fn lowest_present_segment(vfs: &dyn Vfs, wal: &inillucent_wal::Wal) -> u64 {
    let current = wal.sequence();
    for candidate in 1..=current {
        if vfs
            .access(&wal.segment_path(candidate), AccessMode::Exists)
            .unwrap_or(false)
        {
            return candidate;
        }
    }
    current
}

/// Returns what the log says about every page it names.
///
/// @param vfs - the file system the log lives on
/// @param wal - the log, read only for its own identity
fn what_the_log_says(
    vfs: &dyn Vfs,
    wal: &inillucent_wal::Wal,
    checkpoint_lsn: u64,
) -> std::collections::BTreeMap<u64, PageStory> {
    let mut observer = WhatTheLogSays::default();
    let start = RecoveryStart {
        uuid: wal.uuid(),
        // **The file's own checkpoint, not zero.** A record below it is one
        // recovery never reads, so an image down there cannot repair anything
        // and counting it would make this test choose a page the fix could not
        // have helped.
        checkpoint_lsn,
        sequence: lowest_present_segment(vfs, wal),
        cts_watermark: 0,
        doubtful: std::collections::BTreeSet::new(),
    };
    recover(vfs, &DbPath::new(path()), start, &mut observer).expect("the log scans");
    observer.pages
}

/// Builds the fixture and stops without closing, so the window survives.
///
/// **The connection is still open when the snapshot is taken.** Closing one
/// checkpoints its log and retires the segments, which would leave nothing for
/// recovery to replay and nothing for this file to be about. That is what a
/// crash is, and `SimVfs::crash` is how the media is read at that moment.
///
/// The schema and the first rows are checkpointed, so they are behind the
/// window. The rows after it split the table's leaves, which logs the split's
/// three pages whole *after* the row records that filled them - the ordering
/// this file is about.
///
/// @param seed - the media model's seed
/// @param rows - how many rows the table ends with
/// @param width - how long each row's body is, which decides whether the
///   table's leaves split inside the window
fn built(
    seed: u64,
    rows: i64,
    width: usize,
) -> (CrashSnapshot, std::collections::BTreeMap<u64, PageStory>) {
    let vfs = Arc::new(SimVfs::new(SimConfig {
        seed,
        model: MediaModel::default(),
        ..SimConfig::default()
    }));
    let mut engine =
        ImportedDatabase::create_on(Arc::clone(&vfs) as Arc<dyn Vfs>, path(), PAGE_SIZE, FRAMES)
            .expect("the database is created");
    run(
        &mut engine,
        "CREATE TABLE doc (id INTEGER PRIMARY KEY, body TEXT NOT NULL)",
    );
    for id in 1..=6i64 {
        run(
            &mut engine,
            &format!("INSERT INTO doc (id, body) VALUES ({id}, 'row {id}')"),
        );
    }
    engine.checkpoint().expect("the fixture's checkpoint");
    for id in 7..=rows {
        run(
            &mut engine,
            &format!(
                "INSERT INTO doc (id, body) VALUES ({id}, '{}')",
                "x".repeat(width)
            ),
        );
    }
    run(&mut engine, "DELETE FROM doc WHERE id = 3");
    let said = what_the_log_says(vfs.as_ref(), engine.wal(), engine.checkpoint_lsn());
    let snapshot = vfs.crash();
    std::mem::forget(engine);
    (snapshot, said)
}

/// Returns a media holding what the crash left, ready to be torn and opened.
///
/// @param snapshot - what the media held when the power went
/// @param seed - the media model's seed for the recovery run
fn media(snapshot: &CrashSnapshot, seed: u64) -> Arc<SimVfs> {
    Arc::new(SimVfs::recovered(
        SimConfig {
            seed,
            model: MediaModel::default(),
            ..SimConfig::default()
        },
        snapshot,
    ))
}

/// Overwrites one page with a run of `0xA5`.
///
/// Page-sized rubbish rather than a modelled torn write: it fails the page's
/// checksum on any layout, which is the failure a tear leaves, without
/// depending on a device model to produce one.
///
/// @param vfs - the file system the database is on
/// @param page - the page to damage
fn tear(vfs: &Arc<SimVfs>, page: u64) {
    let file = vfs
        .open(&DbPath::new(path()), inillucent_vfs::OpenOptions::main_db())
        .expect("the database file opens");
    let rubbish = vec![0xA5u8; PAGE_SIZE];
    file.write_all_at(page.saturating_mul(PAGE_SIZE as u64), &rubbish)
        .expect("the damage lands");
    file.sync(inillucent_vfs::SyncMode::Full)
        .expect("the damage is durable");
}

/// A page the log holds whole is rebuilt, even when an earlier record reads it.
///
/// **The case the fix is for (task-1962, roadmap item 6).** The page chosen is
/// one the log carries an image of *and* a logical record for, which is the
/// ordering that used to fail: redo reaches the logical record first, reads the
/// torn page and refuses, while the image that would have rebuilt it sits in
/// the same window.
///
/// The rows are asserted afterwards, not only the open: a recovery that
/// rebuilt the page and then lost the records above it would open and answer a
/// shorter table.
#[test]
fn a_page_the_log_holds_whole_is_rebuilt_after_a_record_reads_it() {
    let (snapshot, said) = built(4_100, SPLITTING_ROWS, SPLITTING_WIDTH);
    let vfs = media(&snapshot, 4_150);
    let expected: Vec<i64> = (1..=SPLITTING_ROWS).filter(|id| *id != 3).collect();
    // A page the log carries whole **and** that a record reads, which is the
    // ordering that used to fail: redo reaches the reader first.
    let torn = said
        .iter()
        .find(|(_, held)| held.image.is_some() && held.logical.is_some())
        .map(|(page, _)| *page)
        .unwrap_or_else(|| {
            panic!(
                "no page is both carried whole and read by a record, so the ordering this                  test is about is not in the window: {:?}",
                said.iter()
                    .map(|(page, held)| (*page, held.image, held.logical))
                    .collect::<Vec<_>>()
            )
        });
    tear(&vfs, torn);

    let mut engine =
        ImportedDatabase::open_on(Arc::clone(&vfs) as Arc<dyn Vfs>, path(), PAGE_SIZE, FRAMES)
            .unwrap_or_else(|failure| {
                panic!(
                    "page {torn} is carried whole by a record in the log and the open refused \
                     it anyway: {} ({})",
                    failure.message(),
                    failure.detail().unwrap_or_default()
                )
            });
    assert_eq!(
        ids(&mut engine),
        expected,
        "the database rebuilt page {torn} and then answered different rows"
    );
}

/// A page the log holds nothing for is refused with the documented code.
///
/// **The other half, and the one that stops the first from being a licence to
/// guess.** Recovery rebuilds what the log describes and nothing else; a page
/// no record carries is damage this engine cannot repair, and saying so is what
/// `PRAGMA integrity_check` and every backup procedure depend on. The failure
/// is `SQLITE_CORRUPT` and a message naming the page, not a panic and not a
/// short answer.
#[test]
fn a_page_the_log_holds_nothing_for_is_refused_by_name() {
    // The narrow fixture: no split, so no record in the window carries a page
    // whole and every page it names is one a record only reads.
    let (snapshot, said) = built(4_200, NARROW_ROWS, NARROW_WIDTH);
    let vfs = media(&snapshot, 4_250);
    // A page a record **reads** and no record carries whole: recovery has to
    // go through it and has nothing to rebuild it from.
    let untouched = said
        .iter()
        .find(|(_, held)| held.image.is_none() && held.logical.is_some())
        .map(|(page, _)| *page)
        .unwrap_or_else(|| {
            panic!("every page the log reads is also carried whole, so this case cannot be built")
        });
    tear(&vfs, untouched);

    let failure = match ImportedDatabase::open_on(
        Arc::clone(&vfs) as Arc<dyn Vfs>,
        path(),
        PAGE_SIZE,
        FRAMES,
    ) {
        Ok(mut engine) => {
            // An open that succeeded has to have answered from somewhere,
            // and the only place is the torn page. Reading the table is
            // what turns "it opened" into a statement about the rows.
            let held = ids(&mut engine);
            panic!(
                "page {untouched} is in no record of the log and the open succeeded anyway, \
                     answering {held:?}"
            );
        }
        Err(failure) => failure,
    };
    assert_eq!(
        failure.code(),
        inillucent_base::error::PrimaryCode::Corrupt,
        "damage the log cannot repair is reported as corruption, which is what a \
         caller branches on; it said {:?}",
        failure.detail()
    );
    assert!(
        failure
            .detail()
            .is_some_and(|said| said.contains(&format!("page {untouched}"))),
        "the refusal should name the page it could not read; it said {:?}",
        failure.detail()
    );
}
