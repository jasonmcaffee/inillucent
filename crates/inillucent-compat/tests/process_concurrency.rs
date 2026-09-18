//! Two real writer processes share one database file, and nothing acknowledged
//! is lost.
//!
//! Invariant: **the number of rows in the file equals the number of commits the
//! engine acknowledged.** A child that was refused with a busy error and exited
//! non-zero is a correct outcome and is not counted; a child that exited zero
//! and whose row is missing is the failure this file exists to catch.
//!
//! **Nothing in the tree could see this before (task-1979, section 4).**
//! `concurrency.rs` opens two sessions inside one process, which share a buffer
//! pool, a log handle and a lock. The defect is entirely between processes:
//! each one read the meta record and the log's tail at `open`, before it held
//! the file lock, and then trusted both afterwards - so two processes computed
//! the same append position and each wrote over the other's records. Measured
//! on the build at `3073058`: 120 acknowledged inserts, 60 rows present,
//! `integrity-check ok`, every process exit 0.
//!
//! **Both locking modes, because they failed differently.** Under `exclusive`
//! the lock is never released between statements, so a second process read
//! state from before the first process's whole life; under `normal` the loss
//! was the rows written before another process's checkpoint moved the
//! generation, which the cache discard then threw away.
//!
//! **One process per statement is the shape that matters most.** It is what
//! `inillucent exec` does and what the MCP server does for every tool call, so
//! it is the deployment this engine is most often in rather than a corner.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use inillucent_compat::cliproc::{program, rows, run, Ran};
use inillucent_compat::workspace_root;

/// How many inserts each writer sends in the one-statement-per-process shape.
///
/// Sixty per writer rather than more: the defect this catches lost rows on
/// every round at sixty, and each insert is a process start, so the number is
/// the smallest one that failed reliably rather than the largest one that would
/// have.
const ONE_SHOT_INSERTS: usize = 60;

/// How many inserts each long-lived writer sends.
const LONG_LIVED_INSERTS: usize = 300;

/// Returns a directory of this case's own, emptied first.
///
/// @param name - the case's name, which is also the directory's
fn area(name: &str) -> PathBuf {
    let path = workspace_root()
        .join("_agent_output/process-concurrency")
        .join(name);
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Builds an empty database holding the table both writers insert into.
///
/// **No `INTEGER PRIMARY KEY`, and `UNIQUE(who, n)`.** A missing row in a table
/// with a rowid could be two writers colliding on one rowid, which is a
/// different defect with a different fix; with this shape a row from `a` can
/// never be a row from `b`, so a row that is not there was discarded rather
/// than replaced, and a writer that wrote one twice would be refused rather
/// than silently deduplicated.
///
/// @param binary - the built `inillucent`
/// @param directory - where to put the file
fn prepared(binary: &Path, directory: &Path) -> PathBuf {
    let database = directory.join("shared.rdb");
    let path = database.to_string_lossy().to_string();
    for arguments in [
        vec!["create", path.as_str()],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "CREATE TABLE note (who TEXT NOT NULL, n INTEGER NOT NULL, UNIQUE(who, n))",
        ],
    ] {
        let ran = run(binary, &arguments);
        assert_eq!(
            ran.code,
            0,
            "preparing the database failed at {arguments:?}:\n{}",
            ran.said()
        );
    }
    database
}

/// Returns how many rows the file holds, read by a process that wrote none of
/// them.
///
/// @param binary - the built `inillucent`
/// @param database - the file to read
fn present(binary: &Path, database: &Path) -> usize {
    let ran = run(
        binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "query",
            "SELECT count(*) FROM note",
            "--output",
            "json",
        ],
    );
    assert_eq!(
        ran.code,
        0,
        "counting the rows afterwards failed:\n{}",
        ran.said()
    );
    rows(&ran.stdout)
        .first()
        .and_then(|row| row.first())
        .and_then(|cell| cell.parse::<usize>().ok())
        .unwrap_or_else(|| panic!("the count is not a number:\n{}", ran.stdout))
}

/// Runs one writer's whole sequence of single-statement processes, and returns
/// how many of them exited zero.
///
/// `batch` rather than `exec` because the locking mode has to be set in the
/// same process as the insert, and `exec` runs one statement. The transaction
/// `batch` opens around the two is the same transaction `exec` opens around its
/// one, so the shape under test is unchanged.
///
/// @param binary - the built `inillucent`
/// @param database - the file to write to
/// @param who - which writer this is
/// @param mode - the `locking_mode` each process sets
/// @param inserts - how many inserts to send
fn one_shot_writer(binary: &Path, database: &Path, who: &str, mode: &str, inserts: usize) -> usize {
    let path = database.to_string_lossy().to_string();
    let mut acknowledged = 0usize;
    for n in 1..=inserts {
        let sql = format!(
            "PRAGMA locking_mode = {mode}; INSERT INTO note (who, n) VALUES ('{who}', {n})"
        );
        let ran = run(binary, &["--db", path.as_str(), "batch", sql.as_str()]);
        if ran.code == 0 {
            acknowledged = acknowledged.saturating_add(1);
        }
    }
    acknowledged
}

/// Runs a script through `inillucent-shell` and returns what it produced.
///
/// @param shell - the built `inillucent-shell`
/// @param database - the file to open
/// @param script - the statements, newline separated
fn shell_script(shell: &Path, database: &Path, script: &str) -> Ran {
    let mut child = Command::new(shell)
        .arg(database.to_string_lossy().replace('\\', "/"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("the shell did not start: {error}"));
    if let Some(pipe) = child.stdin.as_mut() {
        let _ = pipe.write_all(script.as_bytes());
    }
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("the shell did not finish: {error}"));
    Ran {
        code: output.status.code().unwrap_or(130),
        stdout: String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        stderr: String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
    }
}

/// Returns how many statements of a shell run were refused.
///
/// A refusal is a correct outcome under contention - it is the busy answer -
/// so the acknowledged count is the statements sent minus the ones that said
/// so, and the assertion is made against that rather than against the number
/// sent.
///
/// @param said - everything the shell wrote to both streams
fn refusals(said: &str) -> usize {
    said.lines()
        .filter(|line| line.to_ascii_lowercase().contains("error"))
        .count()
}

/// Two writers, one process per statement, lose nothing, under both modes.
#[test]
fn two_writer_processes_lose_nothing_one_statement_each() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    for mode in ["normal", "exclusive"] {
        let directory = area(&format!("one-shot-{mode}"));
        let database = prepared(&binary, &directory);
        let acknowledged = std::thread::scope(|scope| {
            let left = {
                let binary = binary.clone();
                let database = database.clone();
                scope
                    .spawn(move || one_shot_writer(&binary, &database, "a", mode, ONE_SHOT_INSERTS))
            };
            let right = {
                let binary = binary.clone();
                let database = database.clone();
                scope
                    .spawn(move || one_shot_writer(&binary, &database, "b", mode, ONE_SHOT_INSERTS))
            };
            let a = left.join().expect("writer a finished");
            let b = right.join().expect("writer b finished");
            a.saturating_add(b)
        });
        assert!(
            acknowledged > 0,
            "mode {mode}: no insert was acknowledged, so this round tested nothing"
        );
        assert_eq!(
            present(&binary, &database),
            acknowledged,
            "mode {mode}: the file does not hold every acknowledged insert"
        );
    }
}

/// Two long-lived writers, autocommitting, lose nothing, under both modes.
#[test]
fn two_writer_processes_lose_nothing_long_lived() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let Some(shell) = program("inillucent-shell") else {
        return;
    };
    for mode in ["normal", "exclusive"] {
        let directory = area(&format!("long-lived-{mode}"));
        let database = prepared(&binary, &directory);
        let script = |who: &str| {
            let mut text = format!("PRAGMA locking_mode = {mode};\n");
            for n in 1..=LONG_LIVED_INSERTS {
                text.push_str(&format!(
                    "INSERT INTO note (who, n) VALUES ('{who}', {n});\n"
                ));
            }
            text
        };
        let acknowledged = std::thread::scope(|scope| {
            let left = {
                let shell = shell.clone();
                let database = database.clone();
                let text = script("a");
                scope.spawn(move || shell_script(&shell, &database, &text))
            };
            let right = {
                let shell = shell.clone();
                let database = database.clone();
                let text = script("b");
                scope.spawn(move || shell_script(&shell, &database, &text))
            };
            let a = left.join().expect("writer a finished");
            let b = right.join().expect("writer b finished");
            LONG_LIVED_INSERTS
                .saturating_sub(refusals(&a.said()))
                .saturating_add(LONG_LIVED_INSERTS.saturating_sub(refusals(&b.said())))
        });
        assert!(
            acknowledged > 0,
            "mode {mode}: no insert was acknowledged, so this round tested nothing"
        );
        assert_eq!(
            present(&binary, &database),
            acknowledged,
            "mode {mode}: the file does not hold every acknowledged insert"
        );
    }
}

/// Two processes with different main databases attaching one shared file lose
/// nothing (task-1979, C2).
#[test]
fn two_processes_attaching_one_file_lose_nothing() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let Some(shell) = program("inillucent-shell") else {
        return;
    };
    let directory = area("attached");
    let shared = prepared(&binary, &directory);
    let own = |who: &str| {
        let path = directory.join(format!("own-{who}.rdb"));
        let ran = run(&binary, &["create", &path.to_string_lossy()]);
        assert_eq!(
            ran.code,
            0,
            "creating {who}'s own database:\n{}",
            ran.said()
        );
        path
    };
    let script = |who: &str| {
        let mut text = format!(
            "ATTACH '{}' AS shared;\n",
            shared.to_string_lossy().replace('\\', "/")
        );
        for n in 1..=LONG_LIVED_INSERTS {
            text.push_str(&format!(
                "INSERT INTO shared.note (who, n) VALUES ('{who}', {n});\n"
            ));
        }
        text
    };
    let left_own = own("a");
    let right_own = own("b");
    let acknowledged = std::thread::scope(|scope| {
        let left = {
            let shell = shell.clone();
            let text = script("a");
            let path = left_own.clone();
            scope.spawn(move || shell_script(&shell, &path, &text))
        };
        let right = {
            let shell = shell.clone();
            let text = script("b");
            let path = right_own.clone();
            scope.spawn(move || shell_script(&shell, &path, &text))
        };
        let a = left.join().expect("writer a finished");
        let b = right.join().expect("writer b finished");
        LONG_LIVED_INSERTS
            .saturating_sub(refusals(&a.said()))
            .saturating_add(LONG_LIVED_INSERTS.saturating_sub(refusals(&b.said())))
    });
    assert!(
        acknowledged > 0,
        "no attached insert was acknowledged, so this case tested nothing"
    );
    assert_eq!(
        present(&binary, &shared),
        acknowledged,
        "the attached file does not hold every acknowledged insert"
    );
}

/// A read only process reads a file a writer has open, rather than waiting out
/// a budget and reporting the writer's lock (task-1979, C5).
#[test]
fn a_readonly_process_reads_while_a_writer_holds_the_file() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let Some(shell) = program("inillucent-shell") else {
        return;
    };
    let directory = area("readonly-reader");
    let database = prepared(&binary, &directory);
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "exec",
            "INSERT INTO note (who, n) VALUES ('a', 1)",
        ],
    );
    assert_eq!(ran.code, 0, "seeding the file:\n{}", ran.said());

    // A writer that opens the file, writes, and then sits on its own standard
    // input with the connection open. The parent reads while it is there.
    let mut writer = Command::new(&shell)
        .arg(database.to_string_lossy().replace('\\', "/"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("the writer did not start: {error}"));
    {
        let pipe = writer.stdin.as_mut().expect("the writer takes input");
        let _ = pipe
            .write_all(b"INSERT INTO note (who, n) VALUES ('a', 2);\nSELECT count(*) FROM note;\n");
        let _ = pipe.flush();
    }
    // Long enough for the child to have opened the file and run its statement.
    std::thread::sleep(std::time::Duration::from_millis(1_500));

    let started = std::time::Instant::now();
    let read = run(
        &binary,
        &[
            "--readonly",
            "--db",
            &database.to_string_lossy(),
            "query",
            "SELECT count(*) FROM note",
            "--output",
            "json",
        ],
    );
    let waited = started.elapsed();
    drop(writer.stdin.take());
    let _ = writer.wait();

    assert_eq!(
        read.code,
        0,
        "a read only process could not read a file a writer has open, after {waited:?}:\n{}",
        read.said()
    );
    let counted = rows(&read.stdout)
        .first()
        .and_then(|row| row.first())
        .and_then(|cell| cell.parse::<u64>().ok())
        .unwrap_or_else(|| panic!("the read only count is not a number:\n{}", read.stdout));
    assert!(
        counted >= 1,
        "the read only process read {counted} rows from a file holding at least one"
    );
}

/// A refusal under contention names who holds the file and what they are doing
/// with it (task-1979, C6).
#[test]
fn a_refusal_names_the_holder_and_the_operation() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let Some(shell) = program("inillucent-shell") else {
        return;
    };
    let directory = area("refusal-text");
    let database = prepared(&binary, &directory);

    // A writer inside an open transaction, which is the one state that holds
    // the file against another writer for longer than a statement.
    let mut writer = Command::new(&shell)
        .arg(database.to_string_lossy().replace('\\', "/"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("the writer did not start: {error}"));
    {
        let pipe = writer.stdin.as_mut().expect("the writer takes input");
        let _ = pipe.write_all(b"BEGIN;\nINSERT INTO note (who, n) VALUES ('a', 1);\n");
        let _ = pipe.flush();
    }
    std::thread::sleep(std::time::Duration::from_millis(1_500));

    let refused = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "exec",
            "INSERT INTO note (who, n) VALUES ('b', 1)",
        ],
    );
    drop(writer.stdin.take());
    let _ = writer.wait();

    assert_ne!(
        refused.code,
        0,
        "a second writer was let in while a transaction was open:\n{}",
        refused.said()
    );
    let said = refused.said().to_ascii_lowercase();
    assert!(
        said.contains("another process"),
        "the refusal does not say another process holds the file:\n{}",
        refused.said()
    );
    assert!(
        said.contains("writing"),
        "the refusal does not say what the holder is doing:\n{}",
        refused.said()
    );
    assert!(
        !said.contains("pending"),
        "the refusal still names an internal lock level:\n{}",
        refused.said()
    );
}
