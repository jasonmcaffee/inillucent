//! A crash mid-checkpoint of the free map's own pages leaves one of two states.
//!
//! Invariant: closes the defect in `Database::checkpoint`
//! (`crates/inillucent-pool/src/file.rs`) - every free-map page is rewritten on
//! **every** checkpoint whether or not it changed, through `Pool::install`
//! alone, with no WAL record and no LSN stamp behind the rewrite. Once an
//! earlier checkpoint's own record for a page's last real change has retired,
//! a crash partway through a later, purely redundant rewrite of that same page
//! has nothing for redo to repair it with - `load_free_map` then fails its
//! checksum on the next open, unconditionally, rather than reading either the
//! state before the checkpoint or the state after it.
//!
//! `inillucent_txn::engine::log_free_map_pages` closes it: every free-map page
//! is logged as a `Body::WritePage` record under transaction `0` - the same
//! "belongs to no transaction, always part of the prefix" convention
//! `AllocPage` and `FreePage` already use, per `should_replay`'s own comment -
//! and stamped with that record's own LSN before it is installed, so
//! `Applier::put_image` repairs a torn copy of it exactly as it repairs a torn
//! tree page.
//!
//! ## Why the two checkpoints and nothing in between
//!
//! The defect is specifically about the **redundant** rewrite: a checkpoint
//! that follows one where nothing has changed. So this builds a fixture,
//! checkpoints it once (the state every attempt below reopens from), and then
//! sweeps every injectable call of a **second**, immediately following
//! checkpoint that touches no row - the free map's own physical page is the
//! only thing that second checkpoint has any reason to rewrite. A crash at
//! every one of its cut points has to reopen to exactly what the first
//! checkpoint left: the same rows, the same bodies, and the same
//! `PRAGMA freelist_count` - asserted as values, not as the absence of a
//! crash - and a fresh allocation afterward has to land somewhere nothing
//! else owns and read back whole, which is what proves the recovered free map
//! is right rather than merely numerically unchanged.

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
use inillucent_wal::record::{Body, Record};
use inillucent_wal::recover::{recover, RecoveryStart, Redo};

/// The page size this fixture builds at, and the frames the pool holds.
///
/// Large enough that nothing is evicted and no checkpoint happens on its own,
/// which is what keeps the checkpoint under test the only one in play.
const PAGE_SIZE: usize = 4_096;
const FRAMES: usize = 8_192;

/// How long each row's body is, so it takes an out-of-line page of its own.
const BODY: usize = 40_000;

/// The table the fixture's rows live in.
const TABLE: &str = "CREATE TABLE doc (id INTEGER PRIMARY KEY, body TEXT NOT NULL)";

/// The ids and fill characters the fixture ends up holding, after two of six
/// rows are deleted - `doc`'s rows once the fixture's own checkpoint runs.
const KEPT: [(i64, char); 4] = [(1, 'a'), (3, 'c'), (5, 'e'), (6, 'f')];

/// Returns the path every run in this file uses.
fn path() -> PathBuf {
    PathBuf::from("free-map-checkpoint-crash.rdb")
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

/// Inserts one wide row, so its body takes a page of its own.
///
/// @param engine - the database
/// @param id - the row's key
/// @param fill - the character its body repeats
fn insert(engine: &mut ImportedDatabase, id: i64, fill: char) {
    let body = fill.to_string().repeat(BODY);
    run(
        engine,
        &format!("INSERT INTO doc (id, body) VALUES ({id}, '{body}')"),
    );
}

/// Deletes one row, freeing the page(s) its body held.
///
/// @param engine - the database
/// @param id - the row's key
fn delete(engine: &mut ImportedDatabase, id: i64) {
    run(engine, &format!("DELETE FROM doc WHERE id = {id}"));
}

/// Returns what `PRAGMA freelist_count` reads.
///
/// @param engine - the database
fn freelist_count(engine: &mut ImportedDatabase) -> i64 {
    let outcome = engine
        .execute_any("PRAGMA freelist_count", &Params::new())
        .expect("freelist_count runs");
    match outcome.rows.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Int(value)) => *value,
        other => panic!("freelist_count answered {other:?}"),
    }
}

/// Returns the ids `doc` holds, in order.
///
/// @param engine - the database
fn ids(engine: &mut ImportedDatabase) -> Vec<i64> {
    let outcome = engine
        .execute_any("SELECT id FROM doc ORDER BY id", &Params::new())
        .expect("doc reads back");
    outcome
        .rows
        .iter()
        .map(|row| match row.first() {
            Some(OwnedDatum::Int(value)) => *value,
            other => panic!("id came back as {other:?}"),
        })
        .collect()
}

/// Every named row's body reads back whole, which a page shared by two rows
/// would not.
///
/// @param engine - the database
/// @param expect - each id and the character its body should repeat
fn assert_bodies_intact(engine: &mut ImportedDatabase, expect: &[(i64, char)]) {
    for (id, fill) in expect {
        let outcome = engine
            .execute_any(
                &format!("SELECT body FROM doc WHERE id = {id}"),
                &Params::new(),
            )
            .unwrap_or_else(|error| panic!("reading doc {id}: {}", error.message()));
        let row = outcome
            .rows
            .first()
            .unwrap_or_else(|| panic!("doc {id} is missing"));
        match row.first() {
            Some(OwnedDatum::Text(bytes)) => assert_eq!(
                String::from_utf8_lossy(bytes),
                fill.to_string().repeat(BODY),
                "doc {id}'s body is not whole"
            ),
            other => panic!("doc {id}'s body came back as {other:?}"),
        }
    }
}

/// Builds the fixture up to its first, unarmed checkpoint.
///
/// Six wide rows with two of them deleted, so the free map already holds
/// something before the checkpoint under test ever runs - a redundant rewrite
/// of an empty map would still be the defect, but this is the ordinary case.
///
/// @param seed - the media model's seed
fn built(seed: u64) -> Arc<SimVfs> {
    let vfs = Arc::new(SimVfs::new(SimConfig {
        seed,
        model: MediaModel::default(),
        ..SimConfig::default()
    }));
    let mut engine =
        ImportedDatabase::create_on(Arc::clone(&vfs) as Arc<dyn Vfs>, path(), PAGE_SIZE, FRAMES)
            .expect("the database is created");
    run(&mut engine, TABLE);
    insert(&mut engine, 1, 'a');
    insert(&mut engine, 2, 'b');
    insert(&mut engine, 3, 'c');
    insert(&mut engine, 4, 'd');
    insert(&mut engine, 5, 'e');
    insert(&mut engine, 6, 'f');
    delete(&mut engine, 2);
    delete(&mut engine, 4);
    engine.checkpoint().expect("the fixture's first checkpoint");
    drop(engine);
    vfs
}

/// What one armed attempt at the *second* checkpoint did.
struct Attempt {
    /// Whether the checkpoint reported success.
    committed: bool,
    /// What the media held when it stopped.
    snapshot: CrashSnapshot,
    /// How many injectable calls the checkpoint reached.
    reached: u64,
}

/// Reopens the fixture and takes a second, redundant checkpoint, with a
/// failure armed at the `n`th injectable call of that checkpoint alone.
///
/// Nothing changes between the two checkpoints, which is the shape the defect
/// is about: the second checkpoint's rewrite of the free map's own page is
/// the purely redundant one `write_free_map` does unconditionally.
///
/// @param seed - the media model's seed for this attempt
/// @param nth - which call of the second checkpoint to fail
fn attempt(seed: u64, nth: u64) -> Attempt {
    let vfs = built(seed);
    let base = vfs.failpoints().sites_reached();
    vfs.failpoints()
        .fail_nth_call(base.saturating_add(nth), Failure::Crash);
    let committed = match ImportedDatabase::open_on(
        Arc::clone(&vfs) as Arc<dyn Vfs>,
        path(),
        PAGE_SIZE,
        FRAMES,
    ) {
        Ok(mut engine) => engine.checkpoint().is_ok(),
        Err(_) => false,
    };
    let reached = vfs.failpoints().sites_reached().saturating_sub(base);
    Attempt {
        committed,
        snapshot: vfs.crash(),
        reached,
    }
}

/// A crash at every cut point of a redundant free-map checkpoint still
/// reopens to exactly the state the checkpoint before it left.
#[test]
fn a_crash_during_a_redundant_free_map_checkpoint_still_reopens() {
    let expected_count = {
        let vfs = built(5_000);
        let mut engine =
            ImportedDatabase::open_on(Arc::clone(&vfs) as Arc<dyn Vfs>, path(), PAGE_SIZE, FRAMES)
                .expect("the fixture reopens");
        assert_eq!(
            ids(&mut engine),
            KEPT.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            "the fixture is exactly what was built"
        );
        assert_bodies_intact(&mut engine, &KEPT);
        freelist_count(&mut engine)
    };
    assert!(
        expected_count > 0,
        "the fixture has to leave something free to be worth testing"
    );

    let mut cut_points = 0u64;
    for nth in 1..=100u64 {
        let outcome = attempt(90_000 + nth, nth);
        if outcome.reached < nth {
            // The failure was armed past the checkpoint's last call, so it
            // never fired and the checkpoint simply succeeded.
            assert!(
                outcome.committed,
                "an unarmed checkpoint must succeed; call {nth} was never reached"
            );
            break;
        }
        cut_points = cut_points.saturating_add(1);
        let recovered: Arc<dyn Vfs> = Arc::new(SimVfs::recovered(
            SimConfig {
                seed: 95_000 + nth,
                model: MediaModel::default(),
                ..SimConfig::default()
            },
            &outcome.snapshot,
        ));
        let mut engine = ImportedDatabase::open_on(recovered, path(), PAGE_SIZE, FRAMES)
            .unwrap_or_else(|error| {
                panic!(
                    "cut {nth}: a crash during a redundant checkpoint left an unreadable \
                     database: {} ({})",
                    error.message(),
                    error.detail().unwrap_or_default()
                )
            });
        assert_eq!(
            ids(&mut engine),
            KEPT.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            "cut {nth}: the rows changed across a checkpoint that changed nothing"
        );
        assert_bodies_intact(&mut engine, &KEPT);
        assert_eq!(
            freelist_count(&mut engine),
            expected_count,
            "cut {nth}: the free page count changed across a checkpoint that changed nothing"
        );
        // The recovered free map is not just numerically unchanged: a fresh
        // allocation still lands somewhere nothing else owns, and reads back
        // whole.
        let fresh_id = 100 + nth as i64;
        insert(&mut engine, fresh_id, 'z');
        assert_bodies_intact(&mut engine, &[(fresh_id, 'z')]);
    }
    assert!(
        cut_points >= 5,
        "a sweep that reached {cut_points} cut points inside the checkpoint is not a sweep"
    );
}

/// Reopens the fixture and takes a second checkpoint, with the free map
/// genuinely changed since the first one and a failure armed at the `n`th
/// injectable call of that checkpoint alone.
///
/// Deletes id 1 and inserts a fresh row before the checkpoint under test, so
/// the free map's own bytes differ from what the first checkpoint installed -
/// the only way to reach `log_free_map_pages`'s `Body::WritePage` branch
/// rather than its "unchanged, skip it" one. `base` is read after those two
/// statements rather than right after `built()`, so their own I/O is not part
/// of the countdown and every `nth` this sweeps lands inside the checkpoint
/// itself, the same as the sibling attempt function above.
///
/// @param seed - the media model's seed for this attempt
/// @param nth - which call of the checkpoint under test to fail
fn attempt_with_a_real_change(seed: u64, nth: u64) -> Attempt {
    let vfs = built(seed);
    let mut engine =
        ImportedDatabase::open_on(Arc::clone(&vfs) as Arc<dyn Vfs>, path(), PAGE_SIZE, FRAMES)
            .expect("the fixture reopens");
    delete(&mut engine, 1);
    insert(&mut engine, FRESH_ID, 'z');
    let base = vfs.failpoints().sites_reached();
    vfs.failpoints()
        .fail_nth_call(base.saturating_add(nth), Failure::Crash);
    let committed = engine.checkpoint().is_ok();
    let reached = vfs.failpoints().sites_reached().saturating_sub(base);
    Attempt {
        committed,
        snapshot: vfs.crash(),
        reached,
    }
}

/// The id a fresh row takes in [`attempt_with_a_real_change`] and its
/// baseline, fixed because every attempt starts from its own fresh `vfs` and
/// there is no collision to avoid across them.
const FRESH_ID: i64 = 999;

/// A crash at every cut point of a free-map checkpoint that *actually
/// changed* the map still reopens to exactly the state that checkpoint would
/// have left.
///
/// **The sibling test above never exercises `log_free_map_pages`'s
/// `Body::WritePage` branch - only proves the "unchanged, skip it" one is
/// safe to crash during.** Its own checkpoint changes nothing between the
/// fixture's first checkpoint and the one under test, on purpose (see this
/// file's module comment), so the fast path fires on every attempt and no
/// `WritePage` record is ever written, let alone torn by a crash. Fable's
/// review of the free-map durability fix (`inillucent-engine/src/checkpoint.rs`)
/// named this gap directly: "checkpoint cut 29" and every other crash sweep in
/// this campaign passed because of the skip, not because the log-and-stamp
/// path was ever exercised.
///
/// Here, deleting id 1 and inserting a fresh row between the reopen and the
/// checkpoint under test changes the free map for real, so this sweep is the
/// first one that can catch a torn `WritePage` record: a crash mid-write used
/// to leave `load_free_map` unable to tell whether the page reflects the
/// state before this checkpoint or the state after it, which is exactly what
/// stamping the image with its own record's LSN before installing it fixes -
/// see `inillucent_txn::engine::log_free_map_pages`.
///
/// **Runs under the default `delete` journal, on purpose - not `off`.** A
/// first version of this test switched to `PRAGMA journal_mode = off` to keep
/// the rollback journal's own hot-journal repair from masking the defect, the
/// same reasoning `durability.rs`'s `Journal::NO_ROLLBACK_JOURNAL` doc comment
/// gives for the no-steal campaigns. That reasoning does not transfer here:
/// `off` is documented (`inillucent-pool/src/journal.rs`) to mean a torn
/// checkpoint page is simply not recoverable, so a test that crashes under
/// `off` and expects recovery anyway is asking for the one thing that mode
/// explicitly does not promise - it found a real, separate gap (a page read
/// before redo can rewrite it, the same shape as the catalog-root circle
/// `open_file` describes; see `docs/roadmap.md`), not the free-map defect
/// this test exists to check. [`a_real_free_map_change_is_logged_as_a_write_page_record`]
/// is what proves the `WritePage` branch runs and is required, directly,
/// without depending on a crash or a journal mode at all - this sweep is
/// still worth keeping under the default mode, because a real free-map
/// change surviving a crash under the mode this engine actually ships with
/// is still worth measuring.
#[test]
fn a_crash_during_a_free_map_checkpoint_that_actually_changed_still_reopens() {
    let (expected_ids, expected_count) = {
        let vfs = built(6_000);
        let mut engine =
            ImportedDatabase::open_on(Arc::clone(&vfs) as Arc<dyn Vfs>, path(), PAGE_SIZE, FRAMES)
                .expect("the fixture reopens");
        delete(&mut engine, 1);
        insert(&mut engine, FRESH_ID, 'z');
        engine
            .checkpoint()
            .expect("the checkpoint under test, unarmed");
        (ids(&mut engine), freelist_count(&mut engine))
    };
    assert!(
        !expected_ids.contains(&1),
        "the baseline's own delete has to have taken effect"
    );

    let mut cut_points = 0u64;
    for nth in 1..=100u64 {
        let outcome = attempt_with_a_real_change(190_000 + nth, nth);
        if outcome.reached < nth {
            assert!(
                outcome.committed,
                "an unarmed checkpoint must succeed; call {nth} was never reached"
            );
            break;
        }
        cut_points = cut_points.saturating_add(1);
        let recovered: Arc<dyn Vfs> = Arc::new(SimVfs::recovered(
            SimConfig {
                seed: 195_000 + nth,
                model: MediaModel::default(),
                ..SimConfig::default()
            },
            &outcome.snapshot,
        ));
        let mut engine = ImportedDatabase::open_on(recovered, path(), PAGE_SIZE, FRAMES)
            .unwrap_or_else(|error| {
                panic!(
                    "cut {nth}: a crash during a free-map checkpoint that actually changed \
                     the map left an unreadable database: {} ({})",
                    error.message(),
                    error.detail().unwrap_or_default()
                )
            });
        assert_eq!(
            ids(&mut engine),
            expected_ids,
            "cut {nth}: the rows do not match the checkpoint that was supposed to commit"
        );
        assert_bodies_intact(
            &mut engine,
            &[(3, 'c'), (5, 'e'), (6, 'f'), (FRESH_ID, 'z')],
        );
        assert_eq!(
            freelist_count(&mut engine),
            expected_count,
            "cut {nth}: the free page count does not match the checkpoint that was supposed \
             to commit"
        );
        // The recovered free map is not just numerically right: a further
        // allocation still lands somewhere nothing else owns, and reads back
        // whole - the same check the sibling test makes.
        let another_id = 300 + nth as i64;
        insert(&mut engine, another_id, 'y');
        assert_bodies_intact(&mut engine, &[(another_id, 'y')]);
    }
    assert!(
        cut_points >= 5,
        "a sweep that reached {cut_points} cut points inside the checkpoint is not a sweep"
    );
}

/// Notes which pages a scanned log's `Body::WritePage` records name.
///
/// A dry run in the sense that matters here: nothing about this touches a
/// data file or rebuilds a page, so it can answer "what did the log record"
/// as a question distinct from "what did recovery do with it" - the two the
/// crash sweeps above conflate, since a mode or a repair pass can make the
/// second one succeed regardless of the first.
#[derive(Default)]
struct WritePagePages {
    /// Every page number a `Body::WritePage` record was found for.
    pages: std::collections::BTreeSet<u64>,
}

impl Redo for WritePagePages {
    /// Always "not yet applied" - this observer rebuilds nothing, so every
    /// record's pages are always worth handing to `redo`.
    ///
    /// @param page - unused
    fn page_lsn(&mut self, _page: u64) -> inillucent_base::DbResult<Option<u64>> {
        Ok(None)
    }

    /// Notes the page a `Body::WritePage` record names; ignores every other
    /// record body, because this observer answers one question only.
    ///
    /// @param record - the record to inspect
    /// @param wanted - unused - every page is always "wanted" (see `page_lsn`)
    fn redo(&mut self, record: &Record<'_>, _wanted: &[bool]) -> inillucent_base::DbResult<()> {
        if let Body::WritePage { page, .. } = record.body {
            self.pages.insert(page);
        }
        Ok(())
    }
}

/// Returns the lowest segment number that still has a file behind it.
///
/// **Neither end of the range is safe to assume.** Segment 1 can already be
/// retired by an earlier checkpoint - `built`'s own fixture checkpoint runs
/// before the one under test - so `RecoveryStart::fresh` (which always names
/// segment 1) can point `read_chain` at a segment that is not there, and an
/// absent first segment ends the chain empty rather than skipping ahead to
/// the next one. Nor is `Wal::sequence` (the segment being written *right
/// now*) safe to assume either: `roll_if_full` can roll to a new segment
/// mid-checkpoint, purely because the current one crossed its size, which
/// puts the checkpoint's own `Checkpoint` marker record in a later segment
/// than the `WritePage` record the same checkpoint appended moments before -
/// measured directly, the first version of this function scanned only
/// `Wal::sequence`'s segment and found the marker record and nothing else.
/// The lowest segment that still exists is the one guaranteed to hold
/// everything from there forward, including whichever segment the
/// `WritePage` record actually landed in.
///
/// @param vfs - the file system the log lives on
/// @param wal - the log, read only for its own current segment number
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

/// Returns which pages a `Body::WritePage` record names, scanning every
/// segment still on disk.
///
/// @param vfs - the file system the log lives on
/// @param wal - the log to scan, read only for its own identity
fn write_page_targets(vfs: &dyn Vfs, wal: &inillucent_wal::Wal) -> std::collections::BTreeSet<u64> {
    let mut observer = WritePagePages::default();
    let start = RecoveryStart {
        uuid: wal.uuid(),
        checkpoint_lsn: 0,
        sequence: lowest_present_segment(vfs, wal),
        cts_watermark: 0,
        doubtful: std::collections::BTreeSet::new(),
    };
    recover(vfs, &DbPath::new(path()), start, &mut observer).expect("the log scans");
    observer.pages
}

/// The checkpoint that changes the free map for real logs a `Body::WritePage`
/// record for it - not only the redundant "unchanged" checkpoint the sibling
/// campaign already proves is safe to crash during.
///
/// **Asserts the branch Codex Sol's review of this ticket found unproven,
/// directly - not through a crash, and not through a journal mode.** The
/// crash sweep above tried proving this by removing the rollback journal
/// (`PRAGMA journal_mode = off`) so its hot-journal repair could not mask a
/// missing `WritePage` record; that reasoning does not transfer from
/// `durability.rs`'s no-steal campaigns, because `off` does not promise a
/// torn checkpoint page is recoverable at all - it found a real, separate gap
/// (write-up in `docs/roadmap.md`) instead of proving anything about the free
/// map. This test asks the narrower, truer question: after a checkpoint that
/// changed the free map, does the log it just wrote contain a `WritePage`
/// record for it, full stop - read straight out of the log with
/// `inillucent_wal::recover`'s `Redo` trait via [`write_page_targets`], which
/// never touches the data file and so cannot be satisfied by any journal or
/// repair mechanism instead of the record itself.
///
/// Checked by hand: removing `log_free_map_pages`'s `wal.append(0,
/// Body::WritePage { .. })` call (and its LSN stamp) makes this assertion
/// fail - the free map still changes correctly in memory and in the file,
/// but the log never says so, which is exactly the gap Fable's review named
/// and the crash sweep above could not, on its own, prove closed.
#[test]
fn a_real_free_map_change_is_logged_as_a_write_page_record() {
    let vfs = built(8_000);
    let mut engine =
        ImportedDatabase::open_on(Arc::clone(&vfs) as Arc<dyn Vfs>, path(), PAGE_SIZE, FRAMES)
            .expect("the fixture reopens");
    // An unrelated transaction, left open through the checkpoint under test.
    // Without this, the free map's own WritePage record is retired as part
    // of the very checkpoint that appended it - the instant its page reaches
    // the file, `recovery_from` advances past the record's own LSN and
    // `retire_segments_below` reclaims the segment holding it, correctly,
    // because nothing left dirty still needs it. That is what a first version
    // of this test found: the log scanned clean every time, fix or no fix,
    // because there was never anything left to find. Holding an unrelated
    // page open pins `recovery_from` below this checkpoint's new records -
    // the free map's own pages are untouched by it and still flush normally -
    // so the segment survives long enough to inspect.
    run(&mut engine, "BEGIN");
    insert(&mut engine, 500, 'x');
    delete(&mut engine, 1);
    insert(&mut engine, FRESH_ID, 'z');
    engine
        .checkpoint()
        .expect("the checkpoint under test, unarmed");
    let logged = write_page_targets(vfs.as_ref(), engine.wal());
    assert!(
        !logged.is_empty(),
        "the checkpoint changed the free map for real, but no Body::WritePage record for it \
         is anywhere in the log"
    );
}

/// One attempt at the same checkpoint with no rollback journal.
///
/// @param seed - what the simulator's randomness starts from
/// @param nth - which injectable call to cut at
fn attempt_with_no_rollback_journal(seed: u64, nth: u64) -> Attempt {
    let vfs = built(seed);
    let mut engine =
        ImportedDatabase::open_on(Arc::clone(&vfs) as Arc<dyn Vfs>, path(), PAGE_SIZE, FRAMES)
            .expect("the fixture reopens");
    run(&mut engine, "PRAGMA journal_mode = off");
    delete(&mut engine, 1);
    insert(&mut engine, FRESH_ID, 'z');
    let base = vfs.failpoints().sites_reached();
    vfs.failpoints()
        .fail_nth_call(base.saturating_add(nth), Failure::Crash);
    let committed = engine.checkpoint().is_ok();
    let reached = vfs.failpoints().sites_reached().saturating_sub(base);
    Attempt {
        committed,
        snapshot: vfs.crash(),
        reached,
    }
}

/// A crash during a free-map checkpoint with no rollback journal still reopens,
/// for every page the log holds a record for.
///
/// **Roadmap item 12, and M3 of the task-1920 review.** `open_file` reads pages
/// before redo has replayed a single record, and a page a crash tore is then
/// read at its torn bytes and fails its checksum - even though the log holds
/// the record that would rebuild it. Under the default `delete` journal the
/// rollback journal's own repair hides this, which is why the sibling sweep
/// above passes and this one is the reproduction: `off` is documented
/// (`inillucent-pool/src/journal.rs`) to mean a torn *checkpoint* page is not
/// recoverable, so this asserts only what that mode still promises - that a
/// page the log describes comes back, and that a database which cannot come
/// back says so rather than answering from a torn page.
///
/// It is not asserting the rows match, for that reason. What it asserts is the
/// distinction the mode makes: a reopen either succeeds and reads every row it
/// claims, or it refuses and names the damage. An open that succeeded and then
/// answered a short table would be the failure, and it is the one this catches.
#[test]
fn a_crash_with_no_rollback_journal_either_reopens_or_says_it_cannot() {
    let mut cut_points = 0u64;
    let mut recovered_cleanly = 0u64;
    let mut refused = 0u64;
    for nth in 1..=100u64 {
        let outcome = attempt_with_no_rollback_journal(390_000 + nth, nth);
        if outcome.reached < nth {
            break;
        }
        cut_points = cut_points.saturating_add(1);
        let media: Arc<dyn Vfs> = Arc::new(SimVfs::recovered(
            SimConfig {
                seed: 395_000 + nth,
                model: MediaModel::default(),
                ..SimConfig::default()
            },
            &outcome.snapshot,
        ));
        match ImportedDatabase::open_on(media, path(), PAGE_SIZE, FRAMES) {
            Ok(mut engine) => {
                recovered_cleanly = recovered_cleanly.saturating_add(1);
                // Every row the table still claims has to read back whole. A
                // short body is the failure a torn page produces when the open
                // succeeded anyway, and it is invisible to a row count.
                let held = ids(&mut engine);
                let bodies: Vec<(i64, char)> = held
                    .iter()
                    .filter_map(|id| match id {
                        2 => Some((2, 'b')),
                        3 => Some((3, 'c')),
                        5 => Some((5, 'e')),
                        6 => Some((6, 'f')),
                        _ => None,
                    })
                    .collect();
                assert_bodies_intact(&mut engine, &bodies);
            }
            Err(_) => refused = refused.saturating_add(1),
        }
    }
    assert!(
        cut_points >= 5,
        "a sweep that reached {cut_points} cut points inside the checkpoint is not a sweep"
    );
    // The counts are in the message because the two ways this can stop being a
    // test read identically without them: a sweep where nothing is ever cut,
    // and one where every cut refuses.
    assert!(
        recovered_cleanly > 0,
        "no cut recovered at all ({cut_points} cuts, {refused} refused), so this sweep is \
         asserting nothing about recovery"
    );
}

/// A page the log holds a record for is rebuilt, even when the open reads it
/// before redo has run.
///
/// **Roadmap item 12, and M3 of the task-1920 review, asked directly.** The
/// crash sweep above cannot ask it: a cut either tears a page the log describes
/// or one it does not, and only the first is recoverable under
/// `journal_mode = off`, so a refusal there is ambiguous. This builds the
/// unambiguous case - a checkpoint that logs a `Body::WritePage` record, and
/// then that exact page overwritten with rubbish on the media underneath - and
/// asserts the reopen rebuilds it from the record.
///
/// The page is chosen from the log rather than named: [`write_page_targets`]
/// reads the log and says which pages it describes, so this cannot go stale by
/// naming page 4 after the layout moves.
///
/// The bytes written are a page-sized run of `0xA5`, which fails the page's
/// checksum on any layout - the failure a torn write leaves, without depending
/// on the device model to produce one.
#[test]
fn a_page_the_log_describes_is_rebuilt_even_though_the_open_reads_it_first() {
    let vfs = built(9_100);
    let logged = {
        let mut engine =
            ImportedDatabase::open_on(Arc::clone(&vfs) as Arc<dyn Vfs>, path(), PAGE_SIZE, FRAMES)
                .expect("the fixture reopens");
        // **The default journal, not `off`.** Under `off` the checkpoint
        // writes no `Body::WritePage` record at all, so there would be nothing
        // for the reopen to rebuild from and the test would be asserting the
        // opposite of what it means to. The eager read this is about happens
        // in every mode.
        //
        // The open transaction is the same device
        // [`a_real_free_map_change_is_logged_as_a_write_page_record`] needs and
        // for the same reason: without something holding `recovery_from` below
        // this checkpoint's own records, the segment carrying the free map's
        // `WritePage` record is retired by the very checkpoint that appended
        // it, and there is nothing left in the log to rebuild from.
        //
        // Its rows are read *before* it opens, because nothing it writes is
        // committed: a recovery rolls the transaction back, so the state this
        // database comes back holding is the one from before the `BEGIN`.
        let expected = ids(&mut engine);
        run(&mut engine, "BEGIN");
        insert(&mut engine, 500, 'x');
        delete(&mut engine, 1);
        insert(&mut engine, FRESH_ID, 'z');
        engine
            .checkpoint()
            .expect("the checkpoint under test, unarmed");
        let logged = write_page_targets(vfs.as_ref(), engine.wal());
        assert!(
            !logged.is_empty(),
            "the checkpoint logged no WritePage record, so there is no page to damage"
        );
        (logged, expected)
    };
    let (pages, expected_ids) = logged;
    let damaged = pages.iter().copied().next().unwrap_or(0);
    assert!(damaged > 0, "page 0 is not a page");

    {
        let file = vfs
            .open(&DbPath::new(path()), inillucent_vfs::OpenOptions::main_db())
            .expect("the database file opens");
        let rubbish = vec![0xA5u8; PAGE_SIZE];
        file.write_all_at(damaged.saturating_mul(PAGE_SIZE as u64), &rubbish)
            .expect("the damage lands");
        file.sync(inillucent_vfs::SyncMode::Full)
            .expect("the damage is durable");
    }

    let mut engine =
        ImportedDatabase::open_on(Arc::clone(&vfs) as Arc<dyn Vfs>, path(), PAGE_SIZE, FRAMES)
            .unwrap_or_else(|failure| {
                panic!(
            "page {damaged} is described by a record in the log and the open refused it anyway: \
             {} ({})",
            failure.message(),
            failure.detail().unwrap_or_default()
        )
            });
    assert_eq!(
        ids(&mut engine),
        expected_ids,
        "the database rebuilt page {damaged} and then answered different rows"
    );
}
