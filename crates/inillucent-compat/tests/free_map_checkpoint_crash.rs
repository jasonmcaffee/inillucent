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
use inillucent_vfs::Vfs;

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
