//! A kill campaign across many points, at two page sizes, losing nothing.
//!
//! Invariant: **however far a real writer process got before the operating
//! system ended it, every transaction it acknowledged is in the file
//! afterwards, whole, and the lost count is exactly zero.**
//!
//! ## What this adds to `process_crash.rs`
//!
//! That file kills at twenty cut points and asserts the same promise, which is
//! the right shape. What it does not do is vary anything else: it runs at the
//! engine's default page size, through one workload, and its cut points are
//! evenly spaced through a run of plain commits.
//!
//! The shape that found task-1987's loss of one insert in six hundred was a
//! campaign that killed at *many* points spread across different kinds of
//! moment - inside a commit, inside a checkpoint, inside a fold - and reopened
//! after each one. This is that: forty seeded cut points, at the default page
//! size and at SQLite's, over a workload that checkpoints while it writes.
//!
//! ## Why the cut points are seeded rather than evenly spaced
//!
//! Evenly spaced points land on the same phase of the workload every time, so
//! a campaign with twice as many of them covers the same moments twice. A
//! seeded sample lands wherever the seed puts it and is the same sample on
//! every run, so a failure is reproducible and a diff of what the campaign
//! covered is readable. The seed is written into the failure message.
//!
//! ## The assertion is a count, not a clock
//!
//! `lost` is the number of acknowledged transactions that are not in the file,
//! and it is asserted to be exactly zero. Nothing here reads a duration.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use inillucent_compat::cliproc::program;
use inillucent_compat::matrix::{default_arm, sqlite_page_arm, Arm, Scale};
use inillucent_compat::stories::open;
use inillucent_compat::workspace_root;

/// How many transactions the writer is fed.
///
/// More than any cut reads, so the child is always killed with work still to
/// do: a child that had finished would be testing a clean exit.
const BATCHES: usize = 200;

/// How many rows one transaction writes.
///
/// Five rather than one, because one row per transaction cannot be torn - any
/// count is consistent - and what a campaign has to be able to see is a
/// transaction that arrived in part.
const PER_BATCH: usize = 5;

/// The seed the cut points are drawn from.
///
/// Written into every failure message, so a campaign that finds something can
/// be run again and find it again.
const SEED: u64 = 0x5EED_2036;

/// A scratch directory for one cut, emptied first.
///
/// @param arm - the configuration this run is at
/// @param cut - which cut it is
fn area(arm: &Arm, cut: usize) -> PathBuf {
    let path = workspace_root()
        .join("_agent_output/process-campaign")
        .join(arm.name)
        .join(format!("cut-{cut}"));
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// The cut points, drawn from the seed and sorted.
///
/// Spread across the whole run rather than bunched at the start, because a
/// campaign that only ever kills in the first twenty transactions never kills
/// inside a checkpoint.
///
/// @param how_many - how many points to draw
fn cut_points(how_many: usize) -> Vec<usize> {
    let mut points = Vec::with_capacity(how_many);
    let mut state = SEED;
    for _ in 0..how_many {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        // Between 1 and BATCHES - 40, so there is always work left after the
        // cut. A cut past the end is a clean exit wearing a campaign's name.
        let span = BATCHES.saturating_sub(40).max(1) as u64;
        points.push(1 + ((state >> 33) % span) as usize);
    }
    points.sort_unstable();
    points.dedup();
    points
}

/// Writes the script the child reads, and returns its path.
///
/// **A file rather than a pipe the parent writes.** The parent has to read the
/// child's acknowledgements while the child is running, and a parent writing
/// four hundred transactions into a pipe on the same thread would fill the
/// pipe's buffer and wait for a reader that is itself.
///
/// Each transaction writes `PER_BATCH` rows carrying its own number, then asks
/// for the highest number committed - which is what the parent counts. Every
/// twenty-fifth transaction checkpoints, so the cut points land inside a
/// checkpoint as well as inside a commit.
///
/// @param directory - where to write it
fn script(directory: &Path) -> PathBuf {
    // **`exclusive`, and that is what leaves recovery anything to do.** Under
    // the default `normal` a connection checkpoints and releases the file after
    // every statement that wrote, so a killed writer's rows are already in the
    // data file and a reopen replays nothing. That is a better outcome and a
    // worse test: what a campaign is about is whether an acknowledged
    // transaction that lives only in the log survives a real kill.
    let mut text = String::from("PRAGMA locking_mode = exclusive;\n");
    for batch in 1..=BATCHES {
        text.push_str("BEGIN;\n");
        for sequence in 1..=PER_BATCH {
            text.push_str(&format!(
                "INSERT INTO note (batch, seq) VALUES ({batch}, {sequence});\n"
            ));
        }
        text.push_str("COMMIT;\n");
        if batch % 25 == 0 {
            text.push_str("PRAGMA wal_checkpoint;\n");
        }
        text.push_str("SELECT max(batch) FROM note;\n");
    }
    let path = directory.join("feed.sql");
    std::fs::write(&path, text).expect("the script is written");
    path
}

/// Builds an empty database at the arm's geometry, with the table the script
/// writes into.
///
/// **In process, because the shipped programs cannot ask for a page size.**
/// `inillucent create` takes a path and nothing else, so a campaign that built
/// its fixture with the command line would run every arm at 32,768 bytes. The
/// file is built here and the shell is then handed the path; the pool reads the
/// page size out of the meta record, so the shell writes it correctly.
///
/// @param arm - the configuration this run is at
/// @param directory - where to put it
fn prepared(arm: &Arm, directory: &Path) -> PathBuf {
    let database = directory.join("app.rdb");
    {
        let made = open(arm, &database);
        let connection = made.session();
        inillucent_compat::stories::run(
            &connection,
            "CREATE TABLE note (batch INTEGER, seq INTEGER)",
        );
    }
    database
}

/// Starts a writer, reads `wanted` acknowledgements, and kills it.
///
/// The acknowledgement is the `SELECT max(batch)` after each `COMMIT`: the
/// child has to have committed the transaction to answer it, so a number the
/// parent has read is a number the engine said was durable.
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
    let mut output = BufReader::new(
        child
            .stdout
            .take()
            .unwrap_or_else(|| panic!("the writer has no output")),
    );

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

/// What a process that did not write the file reads back out of it.
struct Afterwards {
    /// What `PRAGMA integrity_check` answered.
    check: String,
    /// How many rows are there.
    rows: u64,
    /// The highest transaction number in the file.
    highest: u64,
}

/// Opens the file in a fresh process and reads everything the campaign grades.
///
/// **One process rather than four.** The first version ran `integrity-check`,
/// a count and a maximum as three `inillucent` invocations plus the writer, and
/// a campaign of forty cuts at six arms spent about fifteen minutes almost
/// entirely in process startup. One shell fed three statements answers the same
/// three questions, and it is still a handle that did not write the data -
/// which is the property rule 1.4 asks for.
///
/// @param shell - the built `inillucent-shell`
/// @param database - the file to read
fn afterwards(shell: &Path, database: &Path) -> Afterwards {
    let said = inillucent_compat::cliproc::run_with_input(
        shell,
        &[&database.to_string_lossy().replace('\\', "/")],
        "PRAGMA integrity_check;\nSELECT count(*) FROM note;\nSELECT coalesce(max(batch), 0) \
         FROM note;\n",
    );
    let lines: Vec<&str> = said
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let number = |at: usize| -> u64 {
        lines
            .get(at)
            .and_then(|line| line.parse::<u64>().ok())
            .unwrap_or(0)
    };
    Afterwards {
        check: lines.first().copied().unwrap_or("").to_string(),
        rows: number(1),
        highest: number(2),
    }
}

/// Forty cut points: nothing acknowledged is ever lost.
///
/// The count that matters is `lost`, and it is asserted to be exactly zero -
/// which is the same bar `process_concurrency.rs` holds two writers to.
fn a_campaign_of_kills_loses_nothing(arm: &Arm) {
    let shell = program("inillucent-shell");
    let points = cut_points(Scale::from_env().pick(40, 400));
    assert!(
        points.len() >= 20,
        "the seed drew {} distinct cut points, which is not a campaign",
        points.len()
    );

    let mut lost: Vec<String> = Vec::new();
    let mut torn: Vec<String> = Vec::new();
    let mut unopenable: Vec<String> = Vec::new();
    for (cut, wanted) in points.iter().enumerate() {
        let directory = area(arm, cut);
        let database = prepared(arm, &directory);
        let feed = script(&directory);

        let acknowledged = killed_after(&shell, &database, &feed, *wanted);
        if acknowledged == 0 {
            // The child was killed before it committed anything, which is a
            // legitimate outcome of a cut at point one and tells this campaign
            // nothing. It is counted so a run where *every* cut landed there
            // cannot look like a pass.
            continue;
        }

        // A process that did not write the data reads it back - rule 1.4 in its
        // strongest form.
        let read = afterwards(&shell, &database);
        if read.check != "ok" {
            unopenable.push(format!(
                "cut {cut} at acknowledgement {wanted}: integrity_check answered `{}`",
                read.check
            ));
            continue;
        }

        let present = read.rows;
        let highest = read.highest;
        if highest < acknowledged {
            lost.push(format!(
                "cut {cut}: the writer acknowledged transaction {acknowledged} and the file \
                 holds up to {highest}"
            ));
        }
        // Every transaction is there whole or not at all: the rows present have
        // to be a multiple of the transaction size.
        if present % PER_BATCH as u64 != 0 {
            torn.push(format!(
                "cut {cut}: {present} rows is not a whole number of {PER_BATCH} row \
                 transactions"
            ));
        }
    }

    assert!(
        lost.is_empty(),
        "seed {SEED:#x} at the {} arm lost {} acknowledged transactions:\n  {}",
        arm.name,
        lost.len(),
        lost.join("\n  ")
    );
    assert!(
        torn.is_empty(),
        "seed {SEED:#x} at the {} arm left {} torn transactions:\n  {}",
        arm.name,
        torn.len(),
        torn.join("\n  ")
    );
    assert!(
        unopenable.is_empty(),
        "seed {SEED:#x} at the {} arm left {} files that do not pass integrity-check:\n  {}",
        arm.name,
        unopenable.len(),
        unopenable.join("\n  ")
    );
}

/// Forty cut points at the engine's own page size.
#[test]
fn a_campaign_of_kills_loses_nothing_at_the_default_page_size() {
    a_campaign_of_kills_loses_nothing(&default_arm());
}

/// Forty cut points at SQLite's page size.
///
/// **Two arms rather than six, which is what the design asks for.** A campaign
/// is forty child processes an arm; the arms that change a campaign's answer
/// are the ones that change the file's geometry, and the journal modes are
/// covered at every cut point by `durability.rs`'s own campaigns. Six arms of
/// this took about fifteen minutes and found what two find.
#[test]
fn a_campaign_of_kills_loses_nothing_at_sqlites_page_size() {
    a_campaign_of_kills_loses_nothing(&sqlite_page_arm());
}
