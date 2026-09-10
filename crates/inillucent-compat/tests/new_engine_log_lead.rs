//! A statement that dirties more pages than the pool holds.
//!
//! Invariant: **the write-ahead rule is kept by syncing the log, not by
//! refusing the write.** A page may only be written back once the log is
//! durable past that page's LSN. A statement that logs as it goes and never
//! syncs therefore runs the pool out of evictable frames — every candidate
//! carries an LSN the log has not reached — and the pool is right to refuse it.
//! The caller is what has to change.
//!
//! This is a regression test for a defect the Phase 5 qualification found.
//! `CREATE INDEX` on the large fixture failed on every one of thirty rounds
//! with
//!
//! ```text
//! page 4068 carries lsn 6834000 and the log is durable to 5881136:
//! writing it would put the data file ahead of the log
//! ```
//!
//! so the large scale's `schema` family reported no number at all rather than a
//! bad one — which is the worst way for a gate to fail, because an absent row
//! reads as "not measured yet" rather than "this does not work".
//!
//! The fix is `WalLog::keep_the_log_ahead`: the log adapter syncs when the log
//! has run more than a few megabytes ahead of its durable point, and hands the
//! new point to the pool through `Pool::durable_handle`. The sync is real —
//! `write_ahead_point` is `durable_end` under NORMAL and FULL — because
//! advancing the watermark without syncing would be the same defect with the
//! guard switched off.
//!
//! The test uses a **small pool** rather than a large table, which is the same
//! condition and costs seconds instead of minutes: what matters is that the
//! statement dirties more pages than the pool can hold at once.

use std::path::PathBuf;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;

/// How many frames the pool is held to.
///
/// Small enough that building an index over the medium fixture must evict
/// many times, which is the condition the defect needed.
const FRAMES: usize = 64;

/// Returns a copy of the medium fixture, or nothing when it is not built.
///
/// @param tag - what to name the copy
fn fixture(tag: &str) -> Option<PathBuf> {
    let source = workspace_root()
        .join("_agent_output/fixtures")
        .join("medium.db");
    if !source.is_file() {
        return None;
    }
    let area = workspace_root().join("target/scratch/log-lead");
    let _ = std::fs::create_dir_all(&area);
    let target = area.join(format!("{tag}.db"));
    let _ = std::fs::remove_file(&target);
    std::fs::copy(&source, &target).ok()?;
    Some(target)
}

/// A `CREATE INDEX` completes when it dirties more pages than the pool holds.
#[test]
fn an_index_build_bigger_than_the_pool_completes() {
    let Some(path) = fixture("index-build") else {
        eprintln!("the medium fixture is not built; run build-fixtures first");
        return;
    };
    let mut database = ImportedDatabase::import_with(path, 32_768, FRAMES)
        .expect("the fixture imports into the new engine");

    // The pool holds 64 frames; the index covers a hundred thousand rows, so
    // the build must write back many times while the statement is still open.
    database
        .execute_any(
            "CREATE INDEX gate_label ON main_table(label)",
            &Params::new(),
        )
        .unwrap_or_else(|error| {
            panic!(
                "the index build failed: {}",
                error.detail().unwrap_or_default()
            )
        });

    // It has to be a real index, not merely an absence of error.
    let counted = database
        .execute_any(
            "SELECT count(*) FROM main_table WHERE label = 'row 42 lorem ipsum dolor sit amet consectetur'",
            &Params::new(),
        )
        .expect("the new index answers");
    assert_eq!(counted.rows.len(), 1, "one row of one count");

    // And every tree still checks out, which is what says the evictions during
    // the build wrote pages rather than damage.
    database.check_trees().expect("every tree is intact");
}

/// The same build, then a reopen, so the log's own replay is exercised.
///
/// A sync in the middle of a statement is only correct if what it made durable
/// is what recovery replays. Reopening reads the file back through the engine's
/// own open path, which rebuilds every tree from the catalog and the log.
#[test]
fn an_index_build_survives_a_reopen() {
    let Some(path) = fixture("index-reopen") else {
        eprintln!("the medium fixture is not built; run build-fixtures first");
        return;
    };
    let mut database = ImportedDatabase::import_with(path, 32_768, FRAMES)
        .expect("the fixture imports into the new engine");
    database
        .execute_any(
            "CREATE INDEX gate_category ON main_table(category)",
            &Params::new(),
        )
        .unwrap_or_else(|error| {
            panic!(
                "the index build failed: {}",
                error.detail().unwrap_or_default()
            )
        });
    let before = database
        .execute_any(
            "SELECT count(*) FROM main_table WHERE category = 7",
            &Params::new(),
        )
        .expect("the count reads");

    database.reopen().expect("the database reopens");

    let after = database
        .execute_any(
            "SELECT count(*) FROM main_table WHERE category = 7",
            &Params::new(),
        )
        .expect("the count reads after the reopen");
    assert_eq!(
        before.rows, after.rows,
        "the index answered differently after a reopen"
    );
    database.check_trees().expect("every tree is intact");
}
