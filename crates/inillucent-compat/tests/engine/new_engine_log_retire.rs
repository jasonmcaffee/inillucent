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
//!
//! ## Why a statement letting the file go does none of that
//!
//! `locking_mode = normal` is the default, and under it every statement that
//! wrote checkpointed on its way out. That turned all of the above - rolling a
//! segment, writing a checkpoint record, deleting the segments behind it, and
//! rewriting the catalog's statistics - into per-statement work, and it cost a
//! factor of twenty: an autocommit insert went from 1.26 ms to 27.4 ms against
//! SQLite's 4.09 ms on the same fixture, which put three required families
//! under `compat/perf/contract.toml`'s floor (task-1999).
//!
//! What a lock release owes is that the file hold the statement that just
//! succeeded, so that no page of it is left only in this connection's pool
//! while another process has the file. The log's reclamation is not that, and
//! it waits until the log has grown past a thousand pages, or until a
//! checkpoint somebody asked for. The cases below assert three things: that a
//! releasing statement stops doing the housekeeping, that the log it leaves on
//! disk is still bounded by that bar rather than by the length of the run, and
//! that the rows are all there anyway.

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
        // **`exclusive` is asked for, and that is what makes the log window
        // exist (task-1980).** The default is `locking_mode = normal`, under
        // which a connection checkpoints and releases the file after every
        // statement that wrote - so the log never holds more than one
        // statement's worth, and a case about what a checkpoint reclaims has
        // nothing to reclaim. What is under test is the log between
        // checkpoints, so the connection keeps the file the way it did before
        // the default changed.
        "PRAGMA locking_mode = exclusive",
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

/// How many rows the per-statement cases write.
///
/// Enough that the tree is several leaves deep at [`PAGE`], because a case
/// about what the catalog records of a tree's shape cannot tell a recorded one
/// leaf from a real one.
const BY_STATEMENT: usize = 400;

/// Fills a database the way an application does, one autocommit statement at a
/// time under the default `locking_mode = normal`.
///
/// @param database - the database to fill
/// @param count - how many rows to insert
fn fill_by_statement(database: &mut ImportedDatabase, count: usize) {
    database
        .execute_any(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT, c REAL)",
            &Params::new(),
        )
        .expect("the table is created");
    // **An index, because `ImportedDatabase::table_root` finds a table through
    // the covering map**, which is keyed by table root and holds that table's
    // index roots - so a table nothing indexes is not in it and cannot be
    // looked up by name. It also makes every insert move two trees' shapes
    // rather than one, which is the case `refresh_statistics` was rewriting a
    // catalog row for.
    database
        .execute_any("CREATE INDEX t_a ON t(a)", &Params::new())
        .expect("the index is created");
    for nth in 0..count {
        database
            .execute_any(
                &format!(
                    "INSERT INTO t(a,b,c) VALUES ({nth},'row {nth} of a length that fills a \
                     leaf in a reasonable number of rows',1.5)"
                ),
                &Params::new(),
            )
            .expect("the insert runs");
    }
}

/// A statement that lets the file go does not roll a segment.
///
/// **The measurement this is here for (task-1999).** A checkpoint per statement
/// meant a segment created and a directory entry deleted per statement, and
/// that in turn made `Wal::retire_segments_below` walk a sequence range as long
/// as the run - which is where most of a 19 ms checkpoint went. The counter is
/// the log's own `segments`, which counts every segment this handle opened, so
/// a roll that happened cannot be hidden by a deletion that followed it.
///
/// The second half is what keeps the case honest: a checkpoint somebody asked
/// for still rolls one, so this is a claim about *which* checkpoint does the
/// housekeeping rather than about it having been removed.
#[test]
fn a_statement_that_lets_the_file_go_does_not_roll_a_segment() {
    let path = scratch("releasing-does-not-roll");
    let mut database =
        ImportedDatabase::create(path.clone(), PAGE, FRAMES).expect("a fresh database");
    fill_by_statement(&mut database, BY_STATEMENT);
    let after_statements = database.wal().stats().segments;
    assert!(
        after_statements < 16,
        "{BY_STATEMENT} autocommit statements opened {after_statements} log segments, so a \
         statement letting the file go is still rolling one"
    );
    database.checkpoint().expect("the checkpoint runs");
    let after_checkpoint = database.wal().stats().segments;
    assert_eq!(
        after_checkpoint,
        after_statements + 1,
        "a checkpoint somebody asked for has to roll the segment, or the segments behind it can \
         never be retired"
    );
}

/// The log a long autocommit run leaves on disk is bounded by the reclamation
/// bar, not by the length of the run.
///
/// **The first cut of task-1999 failed this, and nothing else would have caught
/// it.** The reclamation was put behind `Wal::checkpoint_due`, whose bar is
/// 256 MiB, on the reasoning that a close would reclaim whatever was left. There
/// is no checkpoint at close anywhere in this engine, so nothing reclaimed at
/// all: 4,000 autocommit statements against a 320 KB database left 129.7 MB of
/// log in two segments after the process exited. Every other case in this file
/// passed, because they all either checkpoint explicitly or ask about what a
/// single statement does.
///
/// It is asserted as a *ratio between two runs* rather than against a byte
/// count, so that it says the property - the log is bounded by the bar, not by
/// the length of the run - without writing the bar's own constant down twice.
/// A run four times as long leaves four times the log when nothing reclaims,
/// and about the same when something does.
#[test]
fn a_long_autocommit_run_does_not_leave_the_whole_log_on_disk() {
    let short_run = 600usize;
    let long_run = short_run * 4;
    let mut left = Vec::new();
    for (tag, count) in [("short", short_run), ("long", long_run)] {
        let path = scratch(&format!("releasing-reclaims-{tag}"));
        let mut database =
            ImportedDatabase::create(path.clone(), PAGE, FRAMES).expect("a fresh database");
        fill_by_statement(&mut database, count);
        // Dropped rather than checkpointed, because a process that exits is
        // exactly the case the bar exists for: nothing in this engine
        // checkpoints when a connection closes.
        drop(database);
        left.push(segments(&path));
    }
    let (short_count, short_bytes) = left.first().copied().unwrap_or_default();
    let (long_count, long_bytes) = left.get(1).copied().unwrap_or_default();
    assert!(
        short_bytes > 0 && long_bytes > 0,
        "neither run left any log at all, so this case is measuring nothing"
    );
    assert!(
        long_bytes <= short_bytes.saturating_mul(2),
        "{long_run} autocommit statements left {long_bytes} bytes of log in {long_count} segments \
         against {short_bytes} in {short_count} for {short_run} statements, so the log is growing \
         with the run rather than being reclaimed"
    );
}

/// A statement that lets the file go does not rewrite the catalog.
///
/// `refresh_statistics` rewrites every catalog row whose tree has changed
/// shape, and an insert moves its tree's shape almost every time, so the guard
/// that skips an unchanged tree never fired: every autocommit insert wrote a
/// catalog delete, a catalog insert and a commit record of its own, inside the
/// checkpoint. It is compared against the same statements under `locking_mode =
/// exclusive`, where no checkpoint runs between statements at all, because that
/// is the number a release has no business being far above.
#[test]
fn a_statement_that_lets_the_file_go_does_not_rewrite_the_catalog() {
    let mut counts = Vec::new();
    for (tag, exclusive) in [("keeping", true), ("releasing", false)] {
        let path = scratch(&format!("catalog-rewrite-{tag}"));
        let mut database =
            ImportedDatabase::create(path.clone(), PAGE, FRAMES).expect("a fresh database");
        if exclusive {
            database
                .execute_any("PRAGMA locking_mode = exclusive", &Params::new())
                .expect("the pragma runs");
        }
        fill_by_statement(&mut database, BY_STATEMENT);
        counts.push(database.wal().stats().records);
    }
    let keeping = counts.first().copied().unwrap_or_default();
    let releasing = counts.get(1).copied().unwrap_or_default();
    assert!(
        releasing <= keeping.saturating_add(2 * BY_STATEMENT as u64),
        "{BY_STATEMENT} statements wrote {releasing} log records when each of them released the \
         file and {keeping} when the connection kept it, which is more than one extra record a \
         statement"
    );
}

/// The rows survive a database abandoned without a clean close.
///
/// The correctness half of the two cases above, and the reason a release still
/// writes the pages and moves the recovery point rather than doing nothing at
/// all: every statement was acknowledged, so every statement has to be there
/// after a process that never closed the file.
#[test]
fn rows_written_a_statement_at_a_time_survive_an_abandoned_database() {
    let path = scratch("releasing-survives");
    let mut database =
        ImportedDatabase::create(path.clone(), PAGE, FRAMES).expect("a fresh database");
    fill_by_statement(&mut database, BY_STATEMENT);
    let expected = rows(&mut database);
    assert_eq!(
        expected, BY_STATEMENT as i64,
        "the fill did not write what it said it did"
    );
    // No checkpoint and no clean close: the handle goes where a crash would
    // have taken the process.
    drop(database);

    let mut recovered =
        ImportedDatabase::open(path.clone(), PAGE, FRAMES).expect("the file reopens");
    assert_eq!(
        rows(&mut recovered),
        expected,
        "rows written one autocommit statement at a time did not survive an abandoned database"
    );
}

/// A checkpoint somebody asked for still makes the statistics honest.
///
/// The statistics stop being rewritten by a release, so the case that they are
/// still written at all is what says the change is a move rather than a
/// removal. It is asserted through what they are for: `PagedTree::attach` seeds
/// a reopened tree's leaf count from the catalog row, so the count a reopened
/// database starts with is exactly what the last checkpoint wrote. The live
/// count before the checkpoint is the value it has to equal, and the case
/// refuses a tree of one leaf, which a stale row and an honest one would agree
/// about.
#[test]
fn a_checkpoint_somebody_asked_for_still_writes_the_statistics() {
    let path = scratch("statistics-on-request");
    let mut database =
        ImportedDatabase::create(path.clone(), PAGE, FRAMES).expect("a fresh database");
    fill_by_statement(&mut database, BY_STATEMENT);
    let root = database.table_root("t").expect("the table has a root page");
    let live = database.leaf_count(root).expect("the tree is attached");
    assert!(
        live > 1,
        "the fill left a tree of {live} leaves, which a stale statistic and an honest one would \
         agree about"
    );
    database.checkpoint().expect("the checkpoint runs");
    drop(database);

    let recovered = ImportedDatabase::open(path.clone(), PAGE, FRAMES).expect("the file reopens");
    let root = recovered
        .table_root("t")
        .expect("the table has a root page after the reopen");
    assert_eq!(
        recovered.leaf_count(root),
        Some(live),
        "the reopened tree was seeded with a leaf count the checkpoint never wrote"
    );
}
