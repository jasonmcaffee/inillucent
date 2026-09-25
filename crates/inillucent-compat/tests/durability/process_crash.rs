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
//! **A cut has to catch the writer with work still in the log** (task-2072).
//! Nothing folds the log into the file during this feed - `release_if_idle`
//! returns early under `PRAGMA locking_mode = exclusive` - so the only fold is
//! the one `Drop for ImportedDatabase` performs when the writer exits on its
//! own. A cut placed beside the end of the feed therefore races the writer to
//! the end of its input, and the race is decided by how long the parent takes
//! to call `TerminateProcess` after reading a line from a pipe. `MARGIN` is
//! what keeps every cut clear of it, `Cut::running` is what says so when it is
//! not, and two `const` assertions beside `MARGIN` stop the ladder growing back
//! into the end of the feed.
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
///
/// **Derived from the ladder rather than chosen.** It was the literal 600 with
/// the ladder clamped to fit, and the clamp is what made this file
/// intermittently red - see [`MARGIN`].
const BATCHES: usize = cut_point(CUTS.saturating_sub(1)).saturating_add(MARGIN);

/// How many batches the writer still has ahead of it at the last cut.
///
/// **What task-2072 was about.** The ladder's top cut is `cut_point(19)`, which
/// is 725, and `BATCHES` was the literal 600 with every cut clamped to
/// `BATCHES - 1`. So cuts 18 and 19 were both 599: the same cut twice, one
/// batch from the end of the feed. The case above then failed at cut 19 about
/// once in three full runs, on the assertion that the reopen reported a
/// recovery.
///
/// **Why the recovery assertion and not one about the rows.** Nothing folds the
/// log into the file during this feed: `release_if_idle` returns early under
/// `PRAGMA locking_mode = exclusive`, so the only fold in the run is
/// `Drop for ImportedDatabase` calling `fold_on_close`, which runs when the
/// writer exits *on its own*. One batch from the end, the writer sometimes
/// finished that batch, read the end of its input and exited before the kill
/// landed, and a writer that exited tidily left nothing for the reopen to
/// replay. The rows were all there; what was missing was a recovery to report.
///
/// **The window, measured.** Driving the same cut from a parent that waits a
/// set time between reading the last acknowledgement and issuing the kill puts
/// the edge at roughly 25 ms: at 10 ms the kill caught the writer mid feed in
/// every run, at 25 ms it sometimes caught it already folded, and at 50 ms the
/// writer had exited cleanly in every run. The feed runs at about 10 ms a
/// batch, which is where the 25 ms comes from. So the test was asking the
/// parent to call `TerminateProcess` within 25 ms of reading a line from a
/// pipe, and an ordinary scheduling delay on a loaded box is longer than that.
///
/// **What 100 buys, measured the same way.** At cut 19 as it now stands, 725 of
/// 825, a parent that waited 1,800 ms still killed a writer with 11 batches to
/// go and work to recover; the writer had exited on its own by 2,200 ms. About
/// 25 ms became about 2,000 ms.
///
/// **It is a count of batches and not a duration on purpose.** What has to be
/// true is that the writer still holds work the log has and the file does not,
/// and work is what a batch is. A faster build shortens the seconds and changes
/// nothing about the property.
const MARGIN: usize = 100;

/// How many rows one transaction writes.
///
/// Five rather than one, because one row per transaction cannot be torn - any
/// count is consistent - and what this file has to be able to see is a
/// transaction that arrived in part.
const PER_BATCH: usize = 5;

/// How many cuts are taken.
const CUTS: usize = 20;

/// Returns how many acknowledgements the Nth cut reads before killing.
///
/// Spread across the run rather than random, so a failure names a cut somebody
/// can reproduce. The points are uneven on purpose: the early ones land before
/// the first checkpoint and the late ones after it.
///
/// **It is not clamped.** A clamp is what turned the top two cuts into one cut
/// beside the end of the feed; `BATCHES` is derived from this function instead,
/// so the ladder decides the feed's length rather than the feed truncating the
/// ladder.
///
/// @param cut - which cut it is
const fn cut_point(cut: usize) -> usize {
    3usize.saturating_add(cut.saturating_mul(cut).saturating_mul(2))
}

/// **Every cut leaves the writer [`MARGIN`] batches of work.**
///
/// A compile error rather than a comment, because the failure the margin
/// prevents is intermittent: a `BATCHES` written back as a literal, or a `CUTS`
/// raised past what the feed covers, would not fail a run. It would make one
/// run in three fail, somewhere else, months later, which is what task-2072
/// cost to diagnose the first time.
const _: () = assert!(
    BATCHES >= cut_point(CUTS.saturating_sub(1)).saturating_add(MARGIN),
    "the last cut must leave MARGIN batches ahead of the writer, or a kill that lands \
     after it reaches the end of the feed finds a tidily closed file - see task-2072"
);

/// **No two cuts are the same cut.**
///
/// Cuts 18 and 19 were both 599 before task-2072, so one of the twenty was
/// spent re-running the other, and a failure naming cut 19 was a failure its
/// neighbour had just passed on identical inputs.
const _: () = assert!(
    cut_point(CUTS.saturating_sub(1)) > cut_point(CUTS.saturating_sub(2)),
    "the cut ladder must be strictly increasing - see task-2072"
);

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

/// What one kill caught the writer doing.
struct Cut {
    /// The highest batch number the writer acknowledged before the kill.
    acknowledged: u64,
    /// Whether the writer was still running its feed when the kill landed.
    ///
    /// **A cut that caught a writer which had already finished is a cut that
    /// tested a clean close** (task-2072). `inillucent-shell` answers 0 when it
    /// reaches the end of its input and closes tidily, and a tidy close folds
    /// the log into the file - so the reopen has nothing to replay and every
    /// assertion this file makes about recovery is being asked of the wrong
    /// state. `Child::kill` is `TerminateProcess(handle, 1)` on Windows and
    /// `SIGKILL` on Unix, so a writer this parent ended answers 1 or no code at
    /// all; only a writer that ended itself answers 0.
    ///
    /// Both cases assert it. It is returned rather than asserted inside
    /// `killed_after` so each can say what its own cut was for.
    running: bool,
}

/// Starts a writer, reads `wanted` acknowledgements, and kills it.
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
fn killed_after(shell: &Path, database: &Path, feed: &Path, wanted: usize) -> Cut {
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
    // **The status is read rather than discarded**, because it is the one thing
    // that says whether this cut caught a writer at all - see [`Cut::running`].
    let status = child
        .wait()
        .unwrap_or_else(|error| panic!("waiting for the writer: {error}"));
    Cut {
        acknowledged,
        running: status.code() != Some(0),
    }
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
    let binary = program("inillucent");
    let shell = program("inillucent-shell");

    for cut in 0..CUTS {
        let directory = area(cut);
        let database = prepared(&binary, &directory);
        let feed = script(&directory);
        let wanted = cut_point(cut);
        let Cut {
            acknowledged,
            running,
        } = killed_after(&shell, &database, &feed, wanted);
        assert!(
            acknowledged > 0,
            "cut {cut}: the writer acknowledged nothing, so this cut tested nothing"
        );
        // **The premise, checked before anything is asked of the file**
        // (task-2072). Every assertion below is about what a kill leaves
        // behind, and none of them means anything if the writer had already
        // reached the end of its feed and closed tidily. The margin in
        // [`MARGIN`] is what makes this hold; this is what says so when it does
        // not, instead of leaving the recovery assertion below to fail with a
        // message about the wrong thing.
        assert!(
            running,
            "cut {cut}: the writer read its whole feed and exited on its own before the kill \
             landed, so this cut tested a tidy close rather than a crash. It read {wanted} of \
             {BATCHES} batches, which should have left {MARGIN} ahead of it"
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
            "cut {cut}: the reopen after a kill did not report that it recovered, although \
             the writer was still running when the kill landed - so the log held nothing \
             above the file's own checkpoint at a cut that read {wanted} of {BATCHES} \
             batches, with {} still ahead of it. Either something folded the log mid feed, \
             which under `PRAGMA locking_mode = exclusive` nothing should, or the writer \
             covered those {} batches in the gap between the parent reading an \
             acknowledgement and the kill landing - see MARGIN, and task-2072 for how that \
             gap was measured:\n{}",
            BATCHES.saturating_sub(wanted),
            BATCHES.saturating_sub(wanted),
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
    let binary = program("inillucent");
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
    let binary = program("inillucent");
    let shell = program("inillucent-shell");
    let directory = area(CUTS);
    let database = prepared(&binary, &directory);
    let feed = script(&directory);
    let Cut {
        acknowledged,
        running,
    } = killed_after(&shell, &database, &feed, 40);
    assert!(
        acknowledged > 0,
        "the writer acknowledged nothing, so this proves nothing"
    );
    // Forty of `BATCHES`, so the writer is nowhere near the end of its feed and
    // the premise is not in doubt - but it is asserted rather than assumed for
    // the same reason as in the case above (task-2072).
    assert!(
        running,
        "the writer exited on its own before the kill landed, so there is no crash here to \
         move the log aside from"
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
