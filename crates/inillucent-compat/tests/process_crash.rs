//! A real writer process is ended by the operating system, and the file is
//! reopened from the parent.
//!
//! Invariant: **every transaction the writer acknowledged is there after the
//! kill, every transaction is there whole or not at all, and the file passes
//! `integrity-check`.** The handle that reads it is in another process, which
//! is the strongest form of rule 1.4 of the testing standard - "durability is
//! asserted by a handle that did not write the data" - and the one form nothing
//! in this workspace exercised (task-1969, 5.6).
//!
//! **Every other crash campaign cuts at a fault the simulator injects.**
//! `wal_crash.rs`, `search_crash.rs`, `vacuum_crash.rs`, `reindex_crash.rs`,
//! `overflow_crash.rs`, `durability.rs` and `new_engine_recovery_shapes.rs` all
//! run inside one process against `SimVfs`, which is the right tool for asking
//! *which* write was lost and the wrong one for asking whether a real operating
//! system ending a real process leaves a file this engine can open. The one
//! real kill in the tree, `inillucent-vfs/tests/conformance.rs`, stops
//! `inillucent-lock-probe` to prove a dead process releases its locks; that is
//! a locking test, not a recovery test.
//!
//! **`Child::kill` is `TerminateProcess` on Windows and `SIGKILL` on Unix.**
//! Neither runs a destructor, flushes a buffer or closes a file: what the
//! kernel has is what survives. That is the behaviour wanted, and it is why
//! this cannot be done with a cooperative shutdown.
//!
//! **What proves it can fail** is `a_cut_with_the_log_moved_aside_loses_the_rows`
//! below. Recovery here is the log segments beside the file; with them moved
//! out of the way the same cut loses not only the acknowledged rows but the
//! table, which is the shape `torn_page_with_image.rs` asserts one layer down
//! by building the damage itself.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use inillucent_compat::cliproc::{document, field, program, rows, run, text_of};
use inillucent_compat::workspace_root;

/// How many transactions the writer is fed.
///
/// More than any cut reads, so the child is always killed with work still to
/// do: a child that had finished would be testing a clean exit.
const BATCHES: usize = 600;

/// How many rows one transaction writes.
///
/// Five rather than one, because one row per transaction cannot be torn - any
/// count is consistent - and what this file has to be able to see is a
/// transaction that arrived in part.
const PER_BATCH: usize = 5;

/// How many cuts are taken.
const CUTS: usize = 20;

/// Returns a directory of this cut's own, emptied first.
///
/// @param cut - which cut it is
fn area(cut: usize) -> PathBuf {
    let path = workspace_root()
        .join("_agent_output/process-crash")
        .join(format!("cut-{cut}"));
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Writes the script the child reads, and returns its path.
///
/// **A file rather than a pipe the parent writes.** The parent has to read the
/// child's acknowledgements while the child is running, and a parent that wrote
/// six hundred transactions into a pipe on the same thread would fill the pipe's
/// buffer and wait for a reader that is itself. A file is the same input with no
/// second thread to get wrong.
///
/// Each transaction writes `PER_BATCH` rows carrying its own number, then asks
/// for the highest number committed - which is what the parent counts.
///
/// @param directory - where to write it
fn script(directory: &Path) -> PathBuf {
    // **`exclusive`, and that is what leaves recovery anything to do
    // (task-1980).** The default is `locking_mode = normal`, under which a
    // connection checkpoints and releases the file after every statement that
    // wrote - so a killed writer's rows are already in the data file and a
    // reopen replays nothing. That is a better outcome and a worse test: what
    // this file is about is whether an acknowledged transaction that lives only
    // in the log survives a real `TerminateProcess`, and under `normal` there
    // is no such transaction to survive. `a_cut_with_the_log_moved_aside_loses_the_rows`
    // is the case that says so out loud: with the log gone the rows must be
    // gone, and under `normal` they are still in the file.
    let mut text = String::from("PRAGMA locking_mode = exclusive;\n");
    for batch in 1..=BATCHES {
        text.push_str("BEGIN;\n");
        for sequence in 1..=PER_BATCH {
            text.push_str(&format!(
                "INSERT INTO note (batch, seq) VALUES ({batch}, {sequence});\n"
            ));
        }
        text.push_str("COMMIT;\n");
        text.push_str("SELECT max(batch) FROM note;\n");
    }
    let path = directory.join("feed.sql");
    std::fs::write(&path, text).expect("the script is written");
    path
}

/// Builds an empty database with the table the script writes into.
///
/// @param binary - the built `inillucent`
/// @param directory - where to put it
fn prepared(binary: &Path, directory: &Path) -> PathBuf {
    let database = directory.join("app.rdb");
    let path = database.to_string_lossy().to_string();
    for arguments in [
        vec!["create", path.as_str()],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "CREATE TABLE note (batch INTEGER, seq INTEGER)",
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

/// Starts a writer, reads `wanted` acknowledgements, and kills it.
///
/// Returns the highest batch number the writer acknowledged before the kill.
///
/// The acknowledgement is the `SELECT max(batch)` after each `COMMIT`: the
/// child has to have committed the transaction to answer it, so a number the
/// parent has read is a number the engine said was durable. That is the promise
/// the assertions below hold it to.
///
/// @param shell - the built `inillucent-shell`
/// @param database - the file to write to
/// @param feed - the script to read
/// @param wanted - how many acknowledgements to read before killing
fn killed_after(shell: &Path, database: &Path, feed: &Path, wanted: usize) -> u64 {
    let input = std::fs::File::open(feed).expect("the script opens");
    let mut child = Command::new(shell)
        .arg(database.to_string_lossy().replace('\\', "/"))
        .stdin(Stdio::from(input))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("the writer did not start: {error}"));
    let mut output = BufReader::new(child.stdout.take().expect("the writer has an output"));

    let mut acknowledged = 0u64;
    let mut read = 0usize;
    while read < wanted {
        let mut line = String::new();
        let got = output
            .read_line(&mut line)
            .unwrap_or_else(|error| panic!("reading the writer's output: {error}"));
        if got == 0 {
            break;
        }
        if let Ok(number) = line.trim().parse::<u64>() {
            acknowledged = number;
            read = read.saturating_add(1);
        }
    }
    // The kill, and nothing before it. No close, no flush, no signal the child
    // could catch: the file on disk is whatever the kernel already had.
    let _ = child.kill();
    let _ = child.wait();
    acknowledged
}

/// Returns one scalar the built binary reads out of a database.
///
/// @param binary - the built `inillucent`
/// @param database - the file to read
/// @param sql - the statement, which must answer one row of one column
fn scalar(binary: &Path, database: &Path, sql: &str) -> String {
    let ran = run(
        binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "query",
            sql,
            "--output",
            "json",
        ],
    );
    assert_eq!(
        ran.code,
        0,
        "reading `{sql}` back after the kill failed:\n{}",
        ran.said()
    );
    rows(&ran.stdout)
        .first()
        .and_then(|row| row.first())
        .cloned()
        .unwrap_or_else(|| panic!("`{sql}` answered no rows:\n{}", ran.stdout))
}

/// Twenty cuts: every acknowledged transaction survives, whole, and the file
/// opens.
#[test]
fn a_killed_writer_leaves_every_acknowledged_transaction_whole() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let Some(shell) = program("inillucent-shell") else {
        return;
    };

    for cut in 0..CUTS {
        let directory = area(cut);
        let database = prepared(&binary, &directory);
        let feed = script(&directory);
        // Spread across the run rather than random, so a failure names a cut
        // somebody can reproduce. The points are uneven on purpose: the early
        // ones land before the first checkpoint and the late ones after it.
        let wanted = 3usize
            .saturating_add(cut.saturating_mul(cut).saturating_mul(2))
            .min(BATCHES.saturating_sub(1));
        let acknowledged = killed_after(&shell, &database, &feed, wanted);
        assert!(
            acknowledged > 0,
            "cut {cut}: the writer acknowledged nothing, so this cut tested nothing"
        );

        // **The first reopen after the kill, and it has to be first.** It is
        // the one that replays the log; once it has, it checkpoints on its way
        // out and every later open finds nothing left to recover. Before
        // task-1980 nothing reported a recovery at all, in text or in
        // `--output json`, so an operator investigating a crash could not ask
        // the tool whether the file had been recovered (task-1979, C10).
        let reported = run(
            &binary,
            &[
                "--db",
                &database.to_string_lossy(),
                "query",
                "SELECT 1",
                "--output",
                "json",
            ],
        );
        assert_eq!(
            reported.code,
            0,
            "cut {cut}: reading the file back failed:\n{}",
            reported.said()
        );
        let object = document(&reported.stdout);
        assert!(
            field(&object, "recovered").is_some(),
            "cut {cut}: the reopen after a kill did not report that it recovered:\n{}",
            reported.stdout
        );
        let said = field(&object, "text").and_then(text_of).unwrap_or_default();
        assert!(
            said.contains("recovered the log"),
            "cut {cut}: the text form does not mention the recovery:\n{said}"
        );

        // The file opens, and the engine says it holds together.
        let checked = run(
            &binary,
            &["--db", &database.to_string_lossy(), "integrity-check"],
        );
        assert_eq!(
            checked.code,
            0,
            "cut {cut}: the file does not pass integrity-check after the kill:\n{}",
            checked.said()
        );

        // Every transaction the writer said was committed is there.
        let highest: u64 = scalar(
            &binary,
            &database,
            "SELECT coalesce(max(batch), 0) FROM note",
        )
        .parse()
        .unwrap_or(0);
        assert!(
            highest >= acknowledged,
            "cut {cut}: the writer acknowledged batch {acknowledged} and the reopened file \
             holds up to {highest}, so a committed transaction was lost"
        );

        // And every transaction that is there is there whole. A batch with
        // fewer than PER_BATCH rows is a transaction that arrived in part,
        // which is the failure a count on its own cannot see.
        let torn = scalar(
            &binary,
            &database,
            &format!(
                "SELECT count(*) FROM (SELECT batch FROM note GROUP BY batch \
                 HAVING count(*) <> {PER_BATCH})"
            ),
        );
        assert_eq!(
            torn, "0",
            "cut {cut}: {torn} transaction(s) are in the reopened file in part"
        );

        // The rows are the ones that were written, not a repetition of one.
        let distinct = scalar(&binary, &database, "SELECT count(DISTINCT batch) FROM note");
        assert_eq!(
            distinct,
            highest.to_string(),
            "cut {cut}: the file holds {distinct} distinct batches and its highest is \
             {highest}, so a batch in the middle is missing"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }
}

/// A reopen that had nothing to recover says nothing about recovery.
///
/// **What keeps the case above from being a test of a constant.** A build that
/// printed the recovery line unconditionally would pass every assertion in this
/// file and tell an operator nothing, which is the state task-1979's C10 found.
#[test]
fn a_clean_reopen_does_not_claim_to_have_recovered() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let directory = area(CUTS.saturating_add(1));
    let database = prepared(&binary, &directory);
    for sequence in 1..=5 {
        let ran = run(
            &binary,
            &[
                "--db",
                &database.to_string_lossy(),
                "exec",
                &format!("INSERT INTO note (batch, seq) VALUES (1, {sequence})"),
            ],
        );
        assert_eq!(ran.code, 0, "seeding the file:\n{}", ran.said());
    }
    let reported = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "query",
            "SELECT count(*) FROM note",
            "--output",
            "json",
        ],
    );
    assert_eq!(reported.code, 0, "reading it back:\n{}", reported.said());
    let object = document(&reported.stdout);
    assert!(
        field(&object, "recovered").is_none(),
        "a clean reopen claimed to have recovered:\n{}",
        reported.stdout
    );
    let _ = std::fs::remove_dir_all(&directory);
}

/// The same cut, with the log moved aside, loses the rows.
///
/// **This is what proves the case above can fail.** Recovery after a kill is
/// the log segments beside the database file; with them moved out of the way
/// the reopened file is the last checkpoint, which for a cut taken early is
/// before the table existed. A run that passed the case above with the log
/// removed would be a run in which recovery was not what put the rows back.
#[test]
fn a_cut_with_the_log_moved_aside_loses_the_rows() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let Some(shell) = program("inillucent-shell") else {
        return;
    };
    let directory = area(CUTS);
    let database = prepared(&binary, &directory);
    let feed = script(&directory);
    let acknowledged = killed_after(&shell, &database, &feed, 40);
    assert!(
        acknowledged > 0,
        "the writer acknowledged nothing, so this proves nothing"
    );

    let held = directory.join("held");
    std::fs::create_dir_all(&held).expect("a directory to hold the log");
    let mut moved = 0usize;
    for entry in std::fs::read_dir(&directory)
        .expect("the directory reads")
        .flatten()
    {
        let path = entry.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if name.contains("-wal.") {
            std::fs::rename(&path, held.join(&name)).expect("the segment moves");
            moved = moved.saturating_add(1);
        }
    }
    assert!(
        moved > 0,
        "the kill left no log segment beside {}, so there was nothing to move and this \
         case is not asking the question it says it is",
        database.display()
    );

    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "query",
            "SELECT coalesce(max(batch), 0) FROM note",
            "--output",
            "json",
        ],
    );
    let found: u64 = if ran.code == 0 {
        rows(&ran.stdout)
            .first()
            .and_then(|row| row.first())
            .and_then(|cell| cell.parse().ok())
            .unwrap_or(0)
    } else {
        // The table itself was in the window, so without the log there is no
        // table to count. That is a stronger loss than a low count and is the
        // usual answer for an early cut.
        0
    };
    assert!(
        found < acknowledged,
        "with {moved} log segment(s) moved aside the reopened file still holds batch \
         {found} against {acknowledged} acknowledged, so recovery is not what put the rows \
         back and the case above is asserting something else"
    );

    let _ = std::fs::remove_dir_all(&directory);
}
