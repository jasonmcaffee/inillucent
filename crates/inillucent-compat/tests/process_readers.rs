//! Four reader processes and one writer, on one file, while it checkpoints.
//!
//! Invariant: **every row set a reader sees is a prefix of the sequence the
//! writer has acknowledged.** Not "the readers did not crash" and not "the
//! counts went up": a reader that saw rows 1, 2 and 4 has seen a state that
//! never existed, and a reader that saw a count of ten with a maximum of twelve
//! has seen half a transaction.
//!
//! ## The shape this is
//!
//! `process_concurrency.rs` runs two writers and counts what was lost.
//! `process_crash.rs` kills one writer and reads it back. Neither runs a reader
//! *while* a writer is working, and a checkpoint under a reader is the second
//! cause task-1987 had - a reader holding a snapshot while the pages under it
//! are being moved into the data file.
//!
//! So: one writer committing in a loop and checkpointing every hundred
//! commits, four readers asking the same question over and over, and every
//! answer graded.
//!
//! ## How a prefix is recognised without a clock
//!
//! The writer inserts `n = 1, 2, 3 …`, one transaction each. A reader's answer
//! is a prefix exactly when
//!
//! - `count(*)` equals `max(n)` - no gaps, because the values are consecutive
//!   from one;
//! - `count(DISTINCT n)` equals `count(*)` - nothing twice;
//! - `min(n)` is 1, or the table is empty.
//!
//! That is four numbers out of one statement, and none of them is a duration.
//! A reader that sees nothing at all is counted too, so a run where every
//! reader started after the writer finished cannot pass for the wrong reason.
//!
//! ## `locking_mode = normal`, deliberately
//!
//! `process_crash.rs` uses `exclusive` because it needs a writer that holds the
//! file, and that is exactly what this must not have: an exclusive writer locks
//! the readers out and there is nothing left to grade. Normal is also the
//! default, so this is the arrangement an application gets.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use inillucent_compat::cliproc::program;
use inillucent_compat::matrix::{default_arm, sqlite_page_arm, Arm, Scale};
use inillucent_compat::stories::{open, run};
use inillucent_compat::workspace_root;

/// How many readers run beside the writer.
const READERS: usize = 4;

/// How often the writer checkpoints, in commits.
const CHECKPOINT_EVERY: usize = 100;

/// A scratch directory for one run, emptied first.
///
/// @param arm - the configuration this run is at
fn area(arm: &Arm) -> PathBuf {
    let path = workspace_root()
        .join("_agent_output/process-readers")
        .join(arm.name);
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Builds the database at the arm's geometry.
///
/// In process, because the shipped programs cannot ask for a page size:
/// `inillucent create` takes a path and nothing else.
///
/// @param arm - the configuration this run is at
/// @param directory - where to put it
fn prepared(arm: &Arm, directory: &Path) -> PathBuf {
    let database = directory.join("shared.rdb");
    {
        let made = open(arm, &database);
        let connection = made.session();
        run(&connection, "CREATE TABLE note (n INTEGER PRIMARY KEY)");
    }
    database
}

/// Writes the writer's script and returns its path.
///
/// One transaction per row, so `n` is the count of acknowledged transactions
/// and a reader's `max(n)` is how far it can see.
///
/// @param directory - where to write it
/// @param commits - how many transactions
fn writer_script(directory: &Path, commits: usize) -> PathBuf {
    let mut text = String::from("PRAGMA locking_mode = normal;\nPRAGMA busy_timeout = 30000;\n");
    for n in 1..=commits {
        text.push_str(&format!("INSERT INTO note (n) VALUES ({n});\n"));
        if n % CHECKPOINT_EVERY == 0 {
            text.push_str("PRAGMA wal_checkpoint;\n");
        }
    }
    text.push_str("SELECT 'writer-is-done';\n");
    let path = directory.join("writer.sql");
    std::fs::write(&path, text).expect("the writer's script is written");
    path
}

/// Writes one reader's script and returns its path.
///
/// The four numbers a prefix is recognised by, asked over and over. An empty
/// table answers `0|0|0|0`, which is a prefix of length zero and is graded as
/// one.
///
/// @param directory - where to write it
/// @param who - which reader it is
/// @param reads - how many times to ask
fn reader_script(directory: &Path, who: usize, reads: usize) -> PathBuf {
    let mut text = String::from("PRAGMA busy_timeout = 30000;\n");
    let probe = "SELECT count(*), coalesce(max(n), 0), count(DISTINCT n), coalesce(min(n), 0) \
                 FROM note;\n";
    for _ in 0..reads {
        text.push_str(probe);
    }
    let path = directory.join(format!("reader-{who}.sql"));
    std::fs::write(&path, text).expect("a reader's script is written");
    path
}

/// Starts a shell reading a script, with its output on a pipe.
///
/// @param shell - the built `inillucent-shell`
/// @param database - the file
/// @param script - the script to read
fn started(shell: &Path, database: &Path, script: &Path) -> Child {
    let input = std::fs::File::open(script).expect("the script opens");
    Command::new(shell)
        .arg(database.to_string_lossy().replace('\\', "/"))
        .stdin(Stdio::from(input))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("a shell did not start: {error}"))
}

/// Waits until the writer's first row is visible, and says whether it appeared.
///
/// **The readers used to start with the writer, and on a loaded machine that
/// made the test about scheduling.** Each reader asks a fixed number of times
/// and then exits; when the machine is busy the writer's first commit can land
/// after a reader has finished all of its asks, and the reader then graded an
/// empty table - which is a legitimate prefix, so nothing was wrong except that
/// the run had checked nothing. One measured failure had one reader of four see
/// a row.
///
/// So the harness waits for the first row and starts the readers after it. The
/// deadline is a guard rather than the measurement: what is asserted is that
/// every reader saw rows, and rule 1.7 is why no assertion here is about how
/// long anything took.
///
/// @param shell - the built `inillucent-shell`
/// @param database - the file the writer is writing
/// @param directory - where the probe script may be written
/// @param within - how long to wait before giving up
fn a_row_exists(shell: &Path, database: &Path, directory: &Path, within: Duration) -> bool {
    let probe = directory.join("probe.sql");
    if std::fs::write(&probe, "SELECT count(*) FROM note;\n").is_err() {
        return false;
    }
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        let Ok(input) = std::fs::File::open(&probe) else {
            return false;
        };
        let answered = Command::new(shell)
            .arg(database.to_string_lossy().replace('\\', "/"))
            .stdin(Stdio::from(input))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();
        if let Ok(output) = answered {
            let said = String::from_utf8_lossy(&output.stdout);
            if said
                .lines()
                .filter_map(|line| line.trim().parse::<u64>().ok())
                .any(|count| count > 0)
            {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// What one reader saw, graded.
struct Seen {
    /// How many answers were read.
    answers: usize,
    /// The highest `max(n)` this reader reached.
    furthest: u64,
    /// The answers that were not a prefix, with what was wrong.
    broken: Vec<String>,
    /// Whether the reader ever saw a row at all.
    saw_a_row: bool,
}

/// Reads a reader's output and grades every answer.
///
/// @param child - the reader
/// @param who - which reader it is, for the failure message
fn graded(mut child: Child, who: usize) -> Seen {
    let Some(output) = child.stdout.take() else {
        panic!("reader {who} has no output");
    };
    let mut seen = Seen {
        answers: 0,
        furthest: 0,
        broken: Vec::new(),
        saw_a_row: false,
    };
    let mut highest_so_far = 0u64;
    for line in BufReader::new(output).lines().map_while(Result::ok) {
        let numbers: Vec<u64> = line
            .trim()
            .split('|')
            .filter_map(|part| part.trim().parse::<u64>().ok())
            .collect();
        let [count, highest, distinct, lowest] = numbers.as_slice() else {
            continue;
        };
        seen.answers = seen.answers.saturating_add(1);
        if *count > 0 {
            seen.saw_a_row = true;
        }
        seen.furthest = seen.furthest.max(*highest);

        if count != highest {
            seen.broken.push(format!(
                "reader {who} saw {count} rows with a highest of {highest}, which has a gap or \
                 half a transaction in it"
            ));
        }
        if distinct != count {
            seen.broken.push(format!(
                "reader {who} saw {count} rows of which {distinct} are distinct, so a row is \
                 there twice"
            ));
        }
        if *count > 0 && *lowest != 1 {
            seen.broken.push(format!(
                "reader {who} saw {count} rows starting at {lowest} rather than at 1, so the \
                 front of the sequence is missing"
            ));
        }
        if *highest < highest_so_far {
            seen.broken.push(format!(
                "reader {who} saw the highest go from {highest_so_far} back to {highest}"
            ));
        }
        highest_so_far = *highest;
    }
    let _ = child.wait();
    seen
}

/// Four readers beside a writer see prefixes and nothing else.
fn readers_beside_a_writer_see_only_prefixes(arm: &Arm) {
    let shell = program("inillucent-shell");
    let directory = area(arm);
    let database = prepared(arm, &directory);
    let commits = Scale::from_env().pick(2_000, 20_000);
    // Enough reads that the readers are still asking when the writer finishes,
    // which is what puts an answer either side of every checkpoint.
    let reads = commits / 2;

    let writer_path = writer_script(&directory, commits);
    let reader_paths: Vec<PathBuf> = (0..READERS)
        .map(|who| reader_script(&directory, who, reads))
        .collect();

    let writer = started(&shell, &database, &writer_path);
    assert!(
        a_row_exists(&shell, &database, &directory, Duration::from_secs(60)),
        "the writer committed no row in a minute at the {} arm, so there was nothing for a \
         reader to see a prefix of",
        arm.name
    );
    let readers: Vec<Child> = reader_paths
        .iter()
        .map(|script| started(&shell, &database, script))
        .collect();

    let mut all_broken: Vec<String> = Vec::new();
    let mut total_answers = 0usize;
    let mut saw_rows = 0usize;
    let mut furthest = 0u64;
    for (who, reader) in readers.into_iter().enumerate() {
        let seen = graded(reader, who);
        total_answers = total_answers.saturating_add(seen.answers);
        if seen.saw_a_row {
            saw_rows = saw_rows.saturating_add(1);
        }
        furthest = furthest.max(seen.furthest);
        all_broken.extend(seen.broken);
    }

    let mut writer = writer;
    let printed = {
        let mut said = String::new();
        if let Some(output) = writer.stdout.take() {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                said.push_str(&line);
                said.push('\n');
            }
        }
        said
    };
    let _ = writer.wait();
    assert!(
        printed.contains("writer-is-done"),
        "the writer did not finish its {commits} commits at the {} arm; it said:\n{}",
        arm.name,
        &printed[..printed.len().min(600)]
    );

    // Rule 1.2 before rule 1.1: a run where the readers answered nothing, or
    // answered only about an empty table, would satisfy every prefix check
    // below and would have graded nothing at all.
    assert!(
        total_answers >= reads,
        "the four readers answered {total_answers} times between them at the {} arm, against \
         {reads} asks each - so they were not reading while the writer wrote",
        arm.name
    );
    assert_eq!(
        saw_rows, READERS,
        "only {saw_rows} of {READERS} readers ever saw a row at the {} arm, so the rest graded \
         an empty table",
        arm.name
    );
    assert!(
        furthest > 0,
        "no reader ever saw a row at the {} arm",
        arm.name
    );

    assert!(
        all_broken.is_empty(),
        "{} of the readers' answers at the {} arm were not a prefix of the acknowledged \
         sequence. A reader that sees a gap has seen a state that never existed:\n  {}",
        all_broken.len(),
        arm.name,
        all_broken
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<String>>()
            .join("\n  ")
    );

    // And the file is sound afterwards, read by a handle that wrote none of it.
    let after = open(arm, &database);
    after
        .check()
        .unwrap_or_else(|why| panic!("the file is not sound afterwards: {}", why.message()));
    let connection = after.session();
    assert_eq!(
        inillucent_compat::stories::ask(&connection, "SELECT count(*), max(n) FROM note"),
        format!("{commits},{commits}"),
        "the writer acknowledged {commits} commits and the file holds something else, at the \
         {} arm",
        arm.name
    );
}

/// Four readers beside a writer, at the engine's own page size.
#[test]
fn readers_see_only_prefixes_at_the_default_page_size() {
    readers_beside_a_writer_see_only_prefixes(&default_arm());
}

/// The same at SQLite's page size.
///
/// **Two arms rather than six.** What changes a reader's answer is the file's
/// geometry and the checkpointing under it, and a run is five concurrent
/// processes writing two thousand transactions. The journal modes are covered
/// at every cut point by `durability.rs`'s campaigns.
#[test]
fn readers_see_only_prefixes_at_sqlites_page_size() {
    readers_beside_a_writer_see_only_prefixes(&sqlite_page_arm());
}
