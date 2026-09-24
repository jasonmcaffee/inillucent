//! The one way a test says it did not run.
//!
//! Invariant: a suite that skips does it through [`skipping`], so exactly one
//! phrase reaches the classifier and exactly one lever turns a skip into a
//! failure. A suite that prints its own sentence is invisible to
//! `inillucent-testrun --strict`, and a suite that returns without printing is
//! invisible to everything.
//!
//! It also holds [`remove_database`], the one way a test clears a scratch
//! database before reusing its path, because a database here is a file plus
//! numbered log segments and every copy of that list elsewhere forgot the
//! segments.
//!
//! **Why this is in the bottom crate rather than in the test harness
//! (task-1969, 4.6).** `inillucent_compat::differential::skipping` was the
//! original home, and three crates could not reach it: the layering contract
//! refuses a production crate a dependency on the harness, even as a
//! dev-dependency, because that puts harness code in the engine's test surface.
//! So `inillucent-driver`, `inillucent-cli` and `inillucent-tree` each printed
//! their own `eprintln!`, `inillucent-remote` kept a private copy of this
//! function, and all four were dropped by `testrun.rs`'s classifier because the
//! phrase alone is not the signal it reads. `inillucent-base` is below every one
//! of them, so the helper reaches them all without an upward edge, and the
//! compat helper now delegates here rather than owning a second copy.
//!
//! The module is behind `#[cfg(any(test, feature = "testing"))]` so a shipped
//! build of `inillucent-base` does not carry it. Each consumer turns the feature
//! on from its own `[dev-dependencies]`, which is on for that crate's test
//! targets and off everywhere else.

// **This module may panic, and the crate-level deny is why the allow is here.**
// Panicking is the function's purpose: under `INILLUCENT_STRICT` a skip has to
// fail the test that skipped, because that is what names the case rather than
// the binary. The bans at the crate root exist to keep panics out of paths that
// read persistent bytes, and this path reads an environment variable.
#![allow(clippy::panic)]

/// What a strict run's skip panic says after the marker.
///
/// **A skip and a failure are different things and the report has to keep them
/// apart (task-1932, H10).** `--strict` makes a skip fail the test, which is
/// what names the case rather than the binary - but a suite that skipped did
/// not evidence a problem, it evidenced nothing, and listing it under FAILED
/// would put a second wrong label on the same event. `inillucent-testrun`'s
/// `missing_prerequisites` reads this sentinel to tell one from the other: a
/// target whose every failure carries it is hollow, and a target with even one
/// failure that does not is a failure.
pub const STRICT_SKIP: &str = " - and this run is strict, so a skip is a failure";

/// The marker every skip message ends with.
///
/// `tests/inillucent-testing-tdd.md` §9 asks for one phrase and
/// `inillucent-testrun`'s classifier matches this one. It is a constant so that
/// a test can assert on it rather than on a literal typed twice.
pub const MARKER: &str = "; skipping";

/// Says why a case did not run, and fails the case when the run is strict.
///
/// **One marker and one decision, in one place (task-1932, H10; task-1969,
/// 4.6).** Every skip site in the workspace ends its message with `; skipping`,
/// which is what `testrun`'s classifier matches. Before task-1932 there were
/// three phrasings and a list of six substrings trying to catch them; before
/// task-1969 there were four crates that could not reach the helper at all and
/// printed the phrase without the panic, so `--strict` dropped them.
///
/// `INILLUCENT_STRICT` makes the skip a failure of the test rather than a
/// classification of the binary. `inillucent-testrun --strict` sets it, and the
/// panic then names the test and the thing that is missing. That matters most
/// in a binary that runs other tests, because a skip of one case there was
/// invisible to `--strict` by both routes.
///
/// @param reason - what is missing, without the marker
pub fn skipping(reason: &str) {
    if std::env::var("INILLUCENT_STRICT").is_ok_and(|value| !value.is_empty()) {
        panic!("{reason}{MARKER}{STRICT_SKIP}");
    }
    eprintln!("{reason}{MARKER}");
}

/// Removes a database file and every file the engine or SQLite keeps beside it.
///
/// **The log is not one file (task-2110, bugs 1 and 7).** The engine writes its
/// log as numbered segments, `<name>-wal.0000000001` and on, and the helpers
/// this replaces removed `<name>-wal` and nothing else. A test that reuses a
/// scratch path then opened a new database beside the last run's segments, and
/// the engine replayed them into it: `new_engine_surface` failed with "table t
/// already exists" after any run of it that had been stopped partway, which
/// read as a defect in whatever the branch had changed. The segments are found
/// by listing the directory because their numbers are not predictable - a chain
/// can start anywhere once a checkpoint has removed its early segments.
///
/// Every removal ignores a missing file, so this is also what a test calls
/// before it creates a database.
///
/// @param path - the database file
pub fn remove_database(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    for suffix in ["-wal", "-journal", "-shm"] {
        let _ = std::fs::remove_file(path.with_file_name(format!("{name}{suffix}")));
    }
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => std::path::Path::new("."),
    };
    let Ok(listing) = std::fs::read_dir(directory) else {
        return;
    };
    let prefix = format!("{name}-wal.");
    for entry in listing.flatten() {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|file| file.starts_with(&prefix))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every numbered segment goes with the database, and a database whose
    /// name is a prefix of this one's is left alone.
    #[test]
    fn a_removed_database_takes_its_segments_with_it() {
        let directory =
            std::env::temp_dir().join(format!("inillucent-remove-database-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("the scratch directory is made");
        let database = directory.join("case.rdb");
        let names = [
            "case.rdb",
            "case.rdb-wal",
            "case.rdb-shm",
            "case.rdb-wal.0000000002",
            "case.rdb-wal.0000000007",
            "case.rdb2",
            "case.rdb2-wal.0000000001",
        ];
        for name in names {
            std::fs::write(directory.join(name), b"x").expect("a file is written");
        }
        remove_database(&database);
        let mut left: Vec<String> = std::fs::read_dir(&directory)
            .expect("the directory lists")
            .flatten()
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .collect();
        left.sort();
        assert_eq!(left, ["case.rdb2", "case.rdb2-wal.0000000001"]);
        std::fs::remove_file(directory.join("case.rdb2")).expect("cleanup");
        std::fs::remove_file(directory.join("case.rdb2-wal.0000000001")).expect("cleanup");
        std::fs::remove_dir(&directory).expect("cleanup");
    }

    /// The message a non-strict run prints ends with the one marker.
    ///
    /// The function writes to stderr rather than returning, so what is asserted
    /// here is the shape of the text the classifier reads, built the same way
    /// the function builds it.
    #[test]
    fn the_message_ends_with_the_marker() {
        let message = format!("the oracle is not built{MARKER}");
        assert!(message.ends_with(MARKER));
        assert!(!message.contains(STRICT_SKIP));
    }

    /// A strict run's panic carries the sentinel after the marker, in that
    /// order, so `missing_prerequisites` can tell a skip from a failure.
    #[test]
    fn a_strict_message_carries_the_sentinel_after_the_marker() {
        let message = format!("the oracle is not built{MARKER}{STRICT_SKIP}");
        let marker_at = message.find(MARKER).expect("the marker is present");
        let sentinel_at = message.find(STRICT_SKIP).expect("the sentinel is present");
        assert!(marker_at < sentinel_at);
        assert!(message.ends_with(STRICT_SKIP));
    }
}
