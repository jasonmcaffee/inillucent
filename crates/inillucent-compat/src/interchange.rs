//! Handing a database from one engine to the other.
//!
//! Invariant: nothing here compares anything or decides anything. It moves a
//! database across the format boundary and returns the path it landed at; every
//! assertion stays in the suite that called it.
//!
//! ## Why this exists
//!
//! inillucent stores an `RDB2` file. SQLite stores a SQLite file. Neither reads
//! the other's pages, and neither was ever going to - that is what
//! `inillucent-migrate` is for in one direction and what this is for in the
//! other. A suite that wanted to ask "does the reference agree with what we
//! wrote" used to hand SQLite the path and let it fail at the header, which
//! tested the file format rather than the schema, and the schema was the
//! question every one of those tests was asking.
//!
//! So the database is moved the way a person would move it: `.dump` renders it
//! as the SQL that rebuilds it, and the reference shell replays that into a file
//! of its own. What survives the trip is exactly what the two engines agree
//! about - the schema text, the rows, the triggers, the views - which is what
//! the suites assert on.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Returns the workspace's own shell, building it first.
///
/// **Built into the directory the calling test's own binary lives in
/// (task-1962).** The shell is looked for beside that binary - `target/debug/`
/// for an ordinary `cargo test`, `target/release/` for a release one, and
/// `target/llvm-cov-target/release/` under `cargo llvm-cov` - and the build
/// used to go to the default target directory whatever the caller's was. Under
/// coverage that built a shell into `target/debug/` and then looked for one in
/// `target/llvm-cov-target/release/`, so twelve `schema_forms` cases failed
/// with "the workspace shell is not built" and a coverage run that cannot
/// finish reports no number at all.
///
/// `None` when the build fails or the binary is not where cargo puts it, which
/// a caller reports rather than works around.
pub fn our_shell() -> Option<PathBuf> {
    let mut directory = std::env::current_exe().ok()?;
    directory.pop();
    directory.pop();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut build = Command::new(cargo);
    build
        .current_dir(crate::workspace_root())
        .args(["build", "-p", "inillucent-cli"]);
    // The profile and the target directory are read back off the path rather
    // than guessed: the last component names the profile, and its parent is
    // what cargo was given as `--target-dir`.
    let profile = directory
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "debug".to_string());
    if profile != "debug" {
        build.args(["--profile", &profile]);
    }
    if let Some(target) = directory.parent() {
        build.arg("--target-dir").arg(target);
    }
    if !build.status().ok()?.success() {
        return None;
    }
    let path = directory.join(format!("inillucent-shell{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Returns the pinned reference shell, when it has been downloaded.
pub fn reference_shell() -> Option<PathBuf> {
    // **`INILLUCENT_SQLITE_SHELL` wins (task-1962, A12).** Four suites carried
    // their own `pinned_shell` with this override in it while this function had
    // none, so setting the variable moved some of the comparison onto a
    // different shell and left the rest on the downloaded one - which is worse
    // than either, because the run would not say so.
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_SHELL") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }
    let directory = crate::workspace_root().join(".sqlite-ref/3.53.4/shell");
    // Both spellings, because a shell copied onto Windows from a release
    // archive is `sqlite3.exe` and one built there by hand is often `sqlite3`.
    let names: [&str; 2] = if cfg!(windows) {
        ["sqlite3.exe", "sqlite3"]
    } else {
        ["sqlite3", "sqlite3.exe"]
    };
    names
        .into_iter()
        .map(|name| directory.join(name))
        .find(|path| path.is_file())
}

/// Returns the SQL that rebuilds an inillucent database.
///
/// The database must not be open: the shell takes the file lock, and this
/// engine's default locking mode is exclusive.
///
/// @param path - the inillucent database to render
pub fn dumped(path: &Path) -> Result<String, String> {
    let shell = our_shell().ok_or("the workspace shell is not built")?;
    let output = Command::new(&shell)
        .arg(path)
        .arg(".dump")
        .output()
        .map_err(|reason| format!("{shell:?}: {reason}"))?;
    if !output.status.success() {
        return Err(format!(
            ".dump failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Returns a database's header pragmas as the statements that restore them.
///
/// **A dump does not carry these and neither engine's does.** `user_version`
/// and `application_id` live in the file header rather than in any table, so a
/// database carried across as SQL arrives with both at zero unless they are
/// asked for and re-set. A test that had just checked `PRAGMA user_version`
/// survived a `VACUUM` read back 0 and blamed the `VACUUM`.
///
/// @param shell - the shell that reads the source
/// @param path - the database to read them from
fn header_pragmas(shell: &Path, path: &Path) -> Result<String, String> {
    let mut restored = String::new();
    for name in ["user_version", "application_id"] {
        let output = Command::new(shell)
            .arg(path)
            .arg(format!("PRAGMA {name};"))
            .output()
            .map_err(|reason| format!("{shell:?}: {reason}"))?;
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if value.is_empty() {
            continue;
        }
        restored.push_str(&format!("PRAGMA {name} = {value};\n"));
    }
    Ok(restored)
}

/// Replays SQL into a database with one of the two shells.
///
/// @param shell - the shell to run
/// @param path - the database it should write
/// @param sql - the statements to replay
fn replay(shell: &Path, path: &Path, sql: &str) -> Result<(), String> {
    let mut child = Command::new(shell)
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|reason| format!("{shell:?}: {reason}"))?;
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().ok_or("the shell took no stdin")?;
        stdin
            .write_all(sql.as_bytes())
            .map_err(|reason| format!("writing the dump: {reason}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|reason| format!("waiting for the shell: {reason}"))?;
    let complaints = String::from_utf8_lossy(&output.stderr);
    // Both shells report a refused statement on stderr and still exit zero, so
    // the exit status alone would let a broken dump through.
    if !complaints.trim().is_empty() {
        return Err(format!("{shell:?} refused the dump: {complaints}"));
    }
    Ok(())
}

/// Rebuilds an inillucent database as a real SQLite file, and returns its path.
///
/// The mirror is written beside the original with `sqlite-mirror` on the name,
/// so a failed run leaves both halves on disk to look at.
///
/// @param path - the inillucent database to carry across
pub fn as_sqlite_file(path: &Path) -> Result<PathBuf, String> {
    let ours = our_shell().ok_or("the workspace shell is not built")?;
    let mut sql = dumped(path)?;
    sql.push_str(&header_pragmas(&ours, path)?);
    let reference = reference_shell().ok_or("the pinned SQLite shell is not downloaded")?;
    let mirror = path.with_extension("sqlite-mirror.db");
    let _ = std::fs::remove_file(&mirror);
    replay(&reference, &mirror, &sql)?;
    if !mirror.is_file() {
        return Err("the reference wrote no file".to_string());
    }
    Ok(mirror)
}

/// Rebuilds a SQLite file as an inillucent database, and returns its path.
///
/// The other half of [`as_sqlite_file`], and what makes a round trip a round
/// trip: a suite that has had the reference *write* to the carried copy gets
/// those writes back in the format it reads.
///
/// @param source - the SQLite file to carry across
/// @param destination - the inillucent database to write
pub fn from_sqlite_file(source: &Path, destination: &Path) -> Result<(), String> {
    let reference = reference_shell().ok_or("the pinned SQLite shell is not downloaded")?;
    let output = Command::new(&reference)
        .arg(source)
        .arg(".dump")
        .output()
        .map_err(|reason| format!("{reference:?}: {reason}"))?;
    if !output.status.success() {
        return Err(format!(
            "the reference .dump failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let mut sql = String::from_utf8_lossy(&output.stdout).into_owned();
    sql.push_str(&header_pragmas(&reference, source)?);
    let ours = our_shell().ok_or("the workspace shell is not built")?;
    let _ = std::fs::remove_file(destination);
    let _ = std::fs::remove_file(destination.with_extension("db-wal"));
    replay(&ours, destination, &sql)?;
    if !destination.is_file() {
        return Err("the workspace shell wrote no file".to_string());
    }
    Ok(())
}
