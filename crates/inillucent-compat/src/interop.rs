//! Reading the databases earlier releases wrote.
//!
//! Invariant: **nothing here ever writes inside `tests/interop/`.** A fixture
//! is what a released binary produced, and the only thing that may produce one
//! is `tools/build-interop-fixture.ps1` running that release. A suite that
//! opened a fixture in place would mutate the evidence by reading it - the
//! engine replays the log on open - so every reader stages a copy first.
//!
//! The directory holds one subdirectory per published release from 0.1.1 on:
//!
//! ```text
//! tests/interop/build.sql           what every release was asked to write
//! tests/interop/verify.sql          what every reader is asked to answer
//! tests/interop/0.1.1/app.rdb       what 0.1.1 wrote
//! tests/interop/0.1.1/app.rdb-wal.0000000002
//! tests/interop/0.1.1/expected.tsv  what 0.1.1 answered
//! ```
//!
//! `build.sql` is deterministic, so every release answers `verify.sql`
//! identically and the six `expected.tsv` files are byte for byte the same.
//! They are kept per release anyway, because a release that answered
//! differently is the finding, and a single shared file could not record which
//! release it was.

use std::fs;
use std::path::{Path, PathBuf};

use crate::workspace_root;

/// One question a reader is asked of a fixture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    /// The label `expected.tsv` records the answer under.
    pub name: String,
    /// The statement, which answers with one row of one column.
    pub sql: String,
}

/// Returns the interop directory.
pub fn directory() -> PathBuf {
    workspace_root().join("tests/interop")
}

/// Returns the schema and rows every release was asked to write.
pub fn build_sql() -> PathBuf {
    directory().join("build.sql")
}

/// Returns the statements that build the searchable graph an older binary is
/// asked to query.
///
/// Run after [`build_sql`] by the backward direction of
/// `release_format_history.rs`, and by nothing else - see the file's own
/// comment for why it is not part of a checked-in fixture.
pub fn retrieval_build_sql() -> PathBuf {
    directory().join("retrieval-build.sql")
}

/// Returns the retrieval questions, in file order.
///
/// These have no `expected.tsv`: they are asked of one database, written by the
/// current build, and the reference answer is the current build's own. See
/// `tests/interop/retrieval.sql`.
pub fn retrieval_questions() -> Vec<Question> {
    parse_questions(&directory().join("retrieval.sql"))
}

/// Returns the questions in `verify.sql`, in file order.
///
/// **One list, and this is the only parse of it in Rust.**
/// `tools/build-interop-fixture.ps1` parses the same file the same way to
/// produce `expected.tsv`, so the two sides cannot drift: a question added to
/// the file is asked by both, and a question worded differently is asked by
/// neither.
pub fn questions() -> Vec<Question> {
    parse_questions(&directory().join("verify.sql"))
}

/// Reads a question file: a `-- name: <label>` line, then one statement.
///
/// @param path - the file to read
fn parse_questions(path: &Path) -> Vec<Question> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut questions = Vec::new();
    let mut name: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("-- name:") {
            name = Some(rest.trim().to_string());
        } else if !trimmed.is_empty() && !trimmed.starts_with("--") {
            if let Some(label) = name.take() {
                questions.push(Question {
                    name: label,
                    sql: trimmed.trim_end_matches(';').to_string(),
                });
            }
        }
    }
    questions
}

/// Returns the released versions that have a fixture, oldest first.
///
/// The order is by number rather than by name, so 0.1.10 would sort after
/// 0.1.9 rather than before it.
pub fn versions() -> Vec<String> {
    let mut found: Vec<(Vec<u32>, String)> = Vec::new();
    let Ok(entries) = fs::read_dir(directory()) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let parts: Vec<u32> = name
            .split('.')
            .filter_map(|part| part.parse::<u32>().ok())
            .collect();
        if parts.len() == 3 && entry.path().join("app.rdb").is_file() {
            found.push((parts, name));
        }
    }
    found.sort();
    found.into_iter().map(|(_, name)| name).collect()
}

/// Returns the answers a release recorded, in file order.
///
/// @param version - the release, such as `0.1.1`
pub fn expected(version: &str) -> Vec<(String, String)> {
    let path = directory().join(version).join("expected.tsv");
    let text = fs::read_to_string(&path).unwrap_or_default();
    text.lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// Copies a release's fixture into a scratch directory and returns the copy.
///
/// **The log segment comes with it, and that is the point of copying at all.**
/// The fixture's newest row was written after a checkpoint, so it exists only
/// in `app.rdb-wal.*`; opening the database replays that log and writes the row
/// into the file, which would leave a checked-in fixture different from the one
/// the release produced. Reading it anywhere else keeps the evidence.
///
/// @param version - the release whose fixture to stage
/// @param into - a scratch directory that this function may write in
pub fn stage(version: &str, into: &Path) -> PathBuf {
    let from = directory().join(version);
    let _ = fs::create_dir_all(into);
    let mut database = into.join("app.rdb");
    let Ok(entries) = fs::read_dir(&from) else {
        return database;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "expected.tsv" {
            continue;
        }
        let destination = into.join(&name);
        if fs::copy(entry.path(), &destination).is_ok() && name == "app.rdb" {
            database = destination;
        }
    }
    database
}

/// Returns a downloaded release's `inillucent` binary, when it is on disk.
///
/// `tools/build-interop-fixture.ps1` puts it beside the cross toolchain, which
/// is gitignored and shared between a checkout and its worktrees - so this
/// looks where that script writes, in the same order it resolves:
/// `INILLUCENT_CROSS_BIN`, then this checkout, then the repository a worktree
/// belongs to.
///
/// @param version - the release, such as `0.1.6`
pub fn release_binary(version: &str) -> Option<PathBuf> {
    for base in cross_bin_candidates() {
        let directory = base.join("releases").join(version).join("extract");
        if let Some(found) = find_binary(&directory, 0) {
            return Some(found);
        }
    }
    None
}

/// Returns the places a downloaded toolchain can be, most specific first.
fn cross_bin_candidates() -> Vec<PathBuf> {
    let mut places = Vec::new();
    if let Ok(named) = std::env::var("INILLUCENT_CROSS_BIN") {
        places.push(PathBuf::from(named));
    }
    places.push(workspace_root().join("tools/cross/bin"));
    // A worktree's own `tools/cross/bin` is empty, and the toolchain lives in
    // the repository it belongs to. `.git` in a worktree is a file naming that
    // repository's directory, so its parent is the main checkout.
    if let Ok(pointer) = fs::read_to_string(workspace_root().join(".git")) {
        if let Some(rest) = pointer.trim().strip_prefix("gitdir:") {
            let git = PathBuf::from(rest.trim());
            if let Some(main) = git.ancestors().find(|path| path.ends_with(".git")) {
                if let Some(checkout) = main.parent() {
                    places.push(checkout.join("tools/cross/bin"));
                }
            }
        }
    }
    places
}

/// Finds `inillucent` under a directory, a few levels down.
///
/// @param directory - where to look
/// @param depth - how far down this call already is
fn find_binary(directory: &Path, depth: usize) -> Option<PathBuf> {
    if depth > 4 {
        return None;
    }
    let named = format!("inillucent{}", std::env::consts::EXE_SUFFIX);
    let entries = fs::read_dir(directory).ok()?;
    let mut directories = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            directories.push(path);
        } else if entry.file_name().to_string_lossy() == named {
            return Some(path);
        }
    }
    directories
        .into_iter()
        .find_map(|path| find_binary(&path, depth.saturating_add(1)))
}
