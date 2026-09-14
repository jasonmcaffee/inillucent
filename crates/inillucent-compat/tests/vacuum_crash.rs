//! Power loss while `VACUUM` is rewriting the whole file.
//!
//! Invariant: **every state a crash can leave a `VACUUM` in reads back as the
//! database it started from or the database it was making, and never as one
//! that has lost a committed row.** `VACUUM` copies every page of a database
//! into a second file and swaps it in, so it is the one statement whose failure
//! can take the whole database with it.
//!
//! ## Why this campaign is on real files and the others are on a simulator
//!
//! `search_crash.rs`, `overflow_crash.rs` and `reindex_crash.rs` arm a failure
//! at the Nth call a run makes to `inillucent-sim`'s VFS. `VACUUM` cannot be cut
//! that way, because `vacuum_in_place` deliberately does not go through the
//! connection's VFS: it writes the rebuilt file beside the original with
//! `std::fs` and swaps it in with `std::fs::rename`, because a rename is a
//! single directory update that a crash cannot catch halfway and the `Vfs`
//! trait has no rename to express that with. `docs/relational-architecture.md`
//! §6 records the decision. Running the statement on a simulated VFS therefore
//! fails outright, with `Open: The system cannot find the path specified`,
//! which is a fact about the design rather than a crash to grade.
//!
//! So this campaign enumerates the states instead of the calls. The sequence
//! has exactly three points a crash can land between - the rebuilt file being
//! written, the rename, and the old log segments being removed - and the
//! rebuilt file is a partial file at every length while it is being written.
//! Each of those is built here out of the statement's own artefacts:
//! `VACUUM INTO` produces the rebuilt bytes through the same `rebuild_into`
//! that `vacuum_in_place` calls, and the rest is the rename and the removal it
//! performs.
//!
//! `crates/inillucent-engine/src/rebuild.rs` has two of these points as unit
//! tests against the private primitives. This is the same question asked from
//! outside, at forty lengths of a half-written rebuild, which is where a
//! recovery that read a truncated file as a database would show up.

use std::path::{Path, PathBuf};

use inillucent_compat::facade::Database;
use inillucent_compat::workspace_root;

/// How many truncation points a half-written rebuild is graded at.
const TRUNCATIONS: usize = 40;

/// The database every run starts from, with enough rows that the delete below
/// frees whole pages and the rebuild has real work to do.
const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER);
     CREATE INDEX t_c ON t(c);
     INSERT INTO t(b, c)
     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 400)
     SELECT hex(zeroblob(200)), i * 7 FROM n;";

/// What runs before the `VACUUM`, so that there is free space to reclaim.
const WORKLOAD: &str = "DELETE FROM t WHERE a > 120;";

/// Returns a scratch directory for one scenario.
///
/// @param name - what to name it after
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root()
        .join("_agent_output/vacuum-crash")
        .join(name);
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::create_dir_all(&directory);
    directory
}

/// Runs statements on a database and closes it tidily.
///
/// @param path - the database file
/// @param sql - the statements
fn run(path: &Path, sql: &str) {
    let database = Database::open(path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    connection.execute_batch(sql).expect("the statements run");
}

/// Returns the state a database reads back as, or why it refused.
///
/// The three queries together are the state: the rows, the index's answer, and
/// the totals. Splitting them would let a database that had lost entries from
/// the index match on the other two.
///
/// @param path - the database file
fn state(path: &Path) -> Result<Vec<String>, String> {
    let database = Database::open(path).map_err(|failure| format!("{failure}"))?;
    let connection = database.connect().map_err(|failure| format!("{failure}"))?;
    let mut rows = Vec::new();
    for query in [
        "SELECT a, length(b), c FROM t ORDER BY a",
        "SELECT a FROM t WHERE c BETWEEN 70 AND 700 ORDER BY c",
        "SELECT count(*), sum(a), sum(c) FROM t",
    ] {
        rows.push(format!("-- {query}"));
        let answered = connection
            .query(query)
            .map_err(|failure| format!("{failure}"))?;
        for row in answered {
            let rendered: Vec<String> = row
                .iter()
                .map(|value| match value.as_integer() {
                    Some(number) => number.to_string(),
                    None => format!("{value:?}"),
                })
                .collect();
            rows.push(rendered.join("|"));
        }
    }
    Ok(rows)
}

/// Returns the log segments beside a database file.
///
/// @param database - the database file
fn log_segments(database: &Path) -> Vec<PathBuf> {
    let Some(directory) = database.parent() else {
        return Vec::new();
    };
    let Some(stem) = database.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let prefix = format!("{stem}-wal.");
    let Ok(listing) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    listing
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .collect()
}

/// Builds a database, frees space in it, and returns it with the state it holds.
///
/// A `VACUUM` changes no answer, so the state below is both the state a crash
/// before it must leave and the state a finished one must leave. That is why
/// every assertion here compares a *reading* of the database rather than its
/// size: a rebuild that lost a row is the failure, and a rebuild that did not
/// compact is not.
///
/// @param directory - where the files go
fn prepared(directory: &Path) -> (PathBuf, Vec<String>) {
    let path = directory.join("main.rdb");
    run(&path, SCHEMA);
    run(&path, WORKLOAD);
    let expected = state(&path).expect("the prepared database reads");
    (path, expected)
}

/// Writes the rebuilt bytes a `VACUUM` would swap in, beside the database.
///
/// `VACUUM INTO` is the same rebuild through the same function -
/// `rebuild_into` - so the file this produces is the one `vacuum_in_place`
/// writes and then renames.
///
/// @param path - the database file
fn rebuilt_beside(path: &Path) -> PathBuf {
    let rebuilt = path.with_extension("rebuilt");
    let _ = std::fs::remove_file(&rebuilt);
    run(
        path,
        &format!(
            "VACUUM INTO '{}'",
            rebuilt.to_string_lossy().replace('\\', "/")
        ),
    );
    rebuilt
}

/// A crash before the rename leaves the database it started from.
///
/// At every length of a half-written rebuild, because that is what the file
/// beside the database looks like while `rebuild_into` is running, and none of
/// them may change what the database answers.
#[test]
fn a_rebuild_cut_short_leaves_the_original_readable() {
    let directory = scratch("before-the-rename");
    let (path, expected) = prepared(&directory);
    let rebuilt = rebuilt_beside(&path);
    let whole = std::fs::read(&rebuilt).expect("the rebuilt file reads back");
    assert!(
        whole.len() > 64 * 1024,
        "the rebuilt file is {} bytes, too small for this to be a campaign",
        whole.len()
    );

    let partial = directory.join("main.rdb-partial");
    for cut in 0..TRUNCATIONS {
        let length = whole.len().saturating_mul(cut).saturating_div(TRUNCATIONS);
        std::fs::write(&partial, whole.get(..length).unwrap_or(&whole))
            .expect("the partial rebuild is written");
        assert_eq!(
            state(&path),
            Ok(expected.clone()),
            "a rebuild cut at {length} bytes changed what the original answers"
        );
    }
    let _ = std::fs::remove_file(&partial);
}

/// A crash after the rename leaves the rebuilt database, not a replay over it.
///
/// The old file's log segments are still beside the new bytes at this point -
/// `vacuum_in_place` removes them only once the rename is durable - and
/// replaying them over the rebuilt file would undo the rebuild. That is what
/// the removal order exists to prevent, so it is asserted from outside here as
/// well as inside `rebuild.rs`.
#[test]
fn a_crash_after_the_rename_leaves_the_rebuilt_database() {
    let directory = scratch("after-the-rename");
    let (path, expected) = prepared(&directory);
    let rebuilt = rebuilt_beside(&path);
    let segments = log_segments(&path);
    std::fs::rename(&rebuilt, &path).expect("the rename lands");
    assert_eq!(
        state(&path),
        Ok(expected),
        "the database after the rename is not the one the rebuild wrote, with {} segments beside it",
        segments.len()
    );
}

/// A crash after the segments are removed leaves the rebuilt database.
#[test]
fn a_crash_after_the_segments_are_removed_leaves_the_rebuilt_database() {
    let directory = scratch("after-the-removal");
    let (path, expected) = prepared(&directory);
    let rebuilt = rebuilt_beside(&path);
    std::fs::rename(&rebuilt, &path).expect("the rename lands");
    for segment in log_segments(&path) {
        let _ = std::fs::remove_file(&segment);
    }
    assert_eq!(
        state(&path),
        Ok(expected),
        "the database with its old segments removed is not the rebuilt one"
    );
}

/// The statement itself, run to the end, answers what it started with.
///
/// The cases above grade the states a crash leaves; this grades the one the
/// statement is supposed to reach, which is what says those are the right two.
#[test]
fn a_vacuum_that_finishes_answers_what_it_started_with() {
    let directory = scratch("finished");
    let (path, expected) = prepared(&directory);
    run(&path, "VACUUM");
    assert_eq!(
        state(&path),
        Ok(expected),
        "a finished VACUUM changed what the database answers"
    );
    // **Nothing here asserts the file got smaller**, and the reason is worth
    // knowing before somebody adds it: the delete above is committed but not
    // checkpointed, so most of what this database holds is in its log segments
    // rather than in the file, and the rebuilt file - which holds all of it -
    // is larger than the one it replaced. Measured: 131,072 bytes before and
    // 262,144 after. Reclamation is `storage.rs`'s question and it asks it
    // after a checkpoint; this campaign's question is whether a row survives.
}
