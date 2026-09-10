//! The log is bounded by the data, and a checkpoint is what bounds it.
//!
//! Invariant: **after a checkpoint the segments on disk hold nothing below the
//! checkpoint LSN** - and everything at or above it is still there, so a
//! database abandoned without a clean close recovers to the same rows whether
//! the process stopped immediately before the retirement or immediately after.
//!
//! `Wal::retire_segments_below` was written, documented as "called after a
//! checkpoint", and covered by six cases in `inillucent-wal/tests/recovery.rs`,
//! and the shipping engine never called it: the one caller was
//! `inillucent-txn`, which is not the engine that ships. The consequence: the
//! same 200,000 rows are 18.4 MB in SQLite and 179.1 MB here, 27.6 MB of data
//! and 151.5 MB of log segments that survive a checkpoint, a clean close, a
//! reopen and a second checkpoint.
//!
//! ## Why a checkpoint also rolls the segment
//!
//! A segment is retirable only once every record in it is below the checkpoint
//! LSN, which the segment *being appended to* never is - the checkpoint record
//! itself lands in it. Calling `retire_segments_below` without first moving the
//! boundary therefore reclaims everything except the current segment, and on
//! the 200,000-row fixture that left 27.5 MB of a finished build on disk for
//! ever. `Wal::roll_segment` moves the boundary to the checkpoint point first,
//! and nothing is deleted until after the data file holds the pages.

use std::path::{Path, PathBuf};

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;

/// The pool size every run here uses.
const FRAMES: usize = 256;

/// The page size every run here uses.
const PAGE: usize = 4_096;

/// Returns a scratch path nothing else in this file uses.
///
/// @param tag - what to name the database
fn scratch(tag: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("log-retire");
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join(format!("{tag}.rdb"));
    forget_files(&path);
    path
}

/// Removes a database and every segment beside it, so a run starts clean.
///
/// @param path - the database file
fn forget_files(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    for sequence in 1..64u64 {
        let _ = std::fs::remove_file(segment_path(path, sequence));
    }
}

/// Returns the path of one log segment beside a database.
///
/// @param path - the database file
/// @param sequence - which segment
fn segment_path(path: &Path, sequence: u64) -> PathBuf {
    let mut held = path.to_path_buf();
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    held.set_file_name(format!("{name}-wal.{sequence:010}"));
    held
}

/// Returns every log segment beside a database, and how many bytes they hold.
///
/// @param path - the database file
fn segments(path: &Path) -> (usize, u64) {
    let mut count = 0usize;
    let mut bytes = 0u64;
    for sequence in 1..64u64 {
        if let Ok(held) = std::fs::metadata(segment_path(path, sequence)) {
            count += 1;
            bytes += held.len();
        }
    }
    (count, bytes)
}

/// Fills a database with enough rows to span several log segments.
///
/// @param database - the database to fill
/// @param doublings - how many times the seed row set is doubled
fn fill(database: &mut ImportedDatabase, doublings: usize) {
    for sql in [
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT, c REAL)",
        "INSERT INTO t(a,b,c) VALUES (1,'seed',1.5)",
    ] {
        database
            .execute_any(sql, &Params::new())
            .unwrap_or_else(|error| panic!("{sql}: {}", error.message()));
    }
    for _ in 0..doublings {
        database
            .execute_any(
                "INSERT INTO t(a,b,c) SELECT a+1, b || 'xxxxxxxxxxxxxxxx', c*1.01 FROM t",
                &Params::new(),
            )
            .expect("the doubling runs");
    }
}

/// Returns how many rows a database's table holds.
///
/// @param database - the database to count
fn rows(database: &mut ImportedDatabase) -> i64 {
    let answer = database
        .execute_any("SELECT count(*) FROM t", &Params::new())
        .expect("the count reads");
    match answer.rows.first().and_then(|row| row.first()) {
        Some(inillucent_tree::datum::OwnedDatum::Int(count)) => *count,
        other => panic!("count(*) answered {other:?}"),
    }
}

/// A checkpoint reclaims the segments the data file has absorbed.
///
/// The measurement, in a test: a build that writes tens of megabytes of log
/// leaves a log smaller than the data it describes once it has been
/// checkpointed. Before the fix the segments were all still there.
#[test]
fn a_checkpoint_reclaims_the_log() {
    let path = scratch("reclaims");
    let mut database =
        ImportedDatabase::create(path.clone(), PAGE, FRAMES).expect("a fresh database");
    fill(&mut database, 13);
    let (before_count, before_bytes) = segments(&path);
    assert!(
        before_bytes > 4 * 1024 * 1024,
        "the build should have written megabytes of log, wrote {before_bytes} in {before_count} segments"
    );
    database.checkpoint().expect("the checkpoint runs");
    let (after_count, after_bytes) = segments(&path);
    let data = std::fs::metadata(&path).map(|held| held.len()).unwrap_or(0);
    assert!(
        after_bytes < before_bytes,
        "the checkpoint reclaimed nothing: {before_bytes} -> {after_bytes}"
    );
    assert!(
        after_bytes < data,
        "the log still holds more than the data file describes: {after_bytes} of log against {data} of data in {after_count} segments"
    );
}

/// A record at or above the checkpoint LSN survives the retirement.
///
/// The half a redo log makes easy to get wrong: retiring by LSN is only safe if
/// the segment holding anything recovery still needs is left alone. It is
/// proved by its consequence - the rows written after the checkpoint are still
/// there after a database that was never closed is opened again.
#[test]
fn work_after_the_checkpoint_survives_an_abandoned_database() {
    let path = scratch("after-checkpoint");
    let mut database =
        ImportedDatabase::create(path.clone(), PAGE, FRAMES).expect("a fresh database");
    fill(&mut database, 10);
    let checkpointed = rows(&mut database);
    database.checkpoint().expect("the checkpoint runs");
    database
        .execute_any(
            "INSERT INTO t(a,b,c) SELECT a+1, b, c FROM t WHERE id <= 100",
            &Params::new(),
        )
        .expect("the write after the checkpoint runs");
    let expected = rows(&mut database);
    assert!(expected > checkpointed, "the second write added no rows");
    // No checkpoint and no clean close: the handle is dropped where a crash
    // would have taken the process, so the only thing that can carry the rows
    // written since the checkpoint is the log.
    drop(database);

    let mut recovered =
        ImportedDatabase::open(path.clone(), PAGE, FRAMES).expect("the file reopens");
    assert_eq!(
        rows(&mut recovered),
        expected,
        "the rows written after the checkpoint did not survive the retirement"
    );
}

/// Stopping immediately before and immediately after a retirement recover the
/// same database.
///
/// The other half. Two runs of exactly the same statements, differing only in
/// whether the checkpoint - and so the deletion - happened, and the rows they
/// come back with have to be identical: a retirement that dropped something
/// recovery needed would show up here as the checkpointed run coming back
/// short.
#[test]
fn a_crash_either_side_of_a_retirement_recovers_the_same_rows() {
    let mut answers = Vec::new();
    for (tag, checkpoint) in [("before-retire", false), ("after-retire", true)] {
        let path = scratch(tag);
        let mut database =
            ImportedDatabase::create(path.clone(), PAGE, FRAMES).expect("a fresh database");
        fill(&mut database, 11);
        if checkpoint {
            database.checkpoint().expect("the checkpoint runs");
        }
        database
            .execute_any("UPDATE t SET a = a + 1 WHERE id <= 500", &Params::new())
            .expect("the update runs");
        database
            .execute_any("DELETE FROM t WHERE id > 1500", &Params::new())
            .expect("the delete runs");
        drop(database);

        let mut recovered =
            ImportedDatabase::open(path.clone(), PAGE, FRAMES).expect("the file reopens");
        let count = rows(&mut recovered);
        let sum = recovered
            .execute_any("SELECT sum(a) FROM t", &Params::new())
            .expect("the sum reads");
        answers.push((count, format!("{:?}", sum.rows)));
    }
    assert_eq!(
        answers.first(),
        answers.get(1),
        "a crash before and after the retirement recovered different databases"
    );
}
