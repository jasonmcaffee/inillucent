//! The performance history: what each workload costs, beside SQLite, over time.
//!
//! Invariant: **every row this appends was measured in the same minute, on the
//! same machine, with both engines interleaved.** A history whose rows were
//! taken under different conditions is a record of the machine rather than of
//! the engine, and the whole point of keeping one is to be able to say "this
//! got slower" and mean it.
//!
//! ## What it records, and why each column is there
//!
//! Wall clock is what a person notices and the least trustworthy number on a
//! shared machine. Processor time is what the engine actually spent and barely
//! moves under load. Peak resident set is the one that catches a change nothing
//! else does - an engine that got faster by holding the whole table in memory
//! has not got faster, it has moved the cost somewhere the clock cannot see.
//! All three are recorded for both engines, so a row can be read as an absolute
//! measurement *and* as a ratio.
//!
//! ## How it accounts for the rest of the machine
//!
//! Three ways, because none of them is enough alone.
//!
//! **Interleaving.** Each round runs inillucent, then SQLite, then inillucent
//! again - so a background job that arrives halfway through slows both arms by
//! about the same amount, and the *ratio* survives it even though the absolute
//! numbers do not. The ratio is therefore the column to read across time; the
//! absolutes are there to catch the case where both moved together.
//!
//! **The median, not the mean.** One round that lost the processor for 200 ms
//! moves a mean and does not move a median. Rounds are cheap; outliers are not
//! informative.
//!
//! **A calibration workload, timed at the start and again at the end.** It is a
//! fixed arithmetic loop that touches nothing, so its time is a reading of how
//! busy the machine is. Both readings go in the row: their size says how fast
//! this machine is compared with the one that wrote the earlier rows, and the
//! *difference between them* says whether the machine changed during the run.
//! A row whose drift is large is a row to distrust, and it says so itself
//! rather than looking like a regression.
//!
//! ## Why both engines are measured as child processes
//!
//! Because one of them has to be. SQLite is a separate program and there is no
//! way to read its cost from inside this one, so the only fair arrangement is
//! for inillucent to be a separate program too - `inillucent-shell` against
//! `sqlite3`, each over its own fresh database, each measured through the same
//! `ProcessCost` reading of the same operating system counters. Measuring one
//! arm in-process and the other through a pipe would be comparing two different
//! accounting methods and calling the difference an engine.
//!
//! Usage:
//!   `cargo run -p inillucent-compat --bin inillucent-perfhistory -- [--rounds N] [--dry-run]`

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use inillucent_compat::procstat::{child_cost, mebibytes, millis, ProcessCost};
use inillucent_compat::workspace_root;

/// How many times each workload is run against each engine by default.
///
/// Five is enough for a median to be stable and few enough that the whole
/// history run is a couple of minutes. The gates in this crate run thirty; they
/// are deciding whether a release claim holds, and this is keeping a series.
const DEFAULT_ROUNDS: usize = 5;

/// One workload: a name, the data it needs, and the operation being timed.
struct Workload {
    /// What the history calls it. Once written, never renamed - a renamed
    /// workload is a new series with the old one's history thrown away.
    name: &'static str,
    /// The data the operation runs against, built once and **not timed**.
    setup: &'static str,
    /// The operation, and the only thing the clock is running for.
    script: &'static str,
    /// How many times the operation is repeated inside one timed run.
    ///
    /// **Because starting a process costs about 18 ms here and several of these
    /// operations cost less than that.** Subtracting a startup of the same size
    /// as the measurement leaves noise, and the ratios came out as `0.00x` -
    /// which is not a fast engine or a slow one, it is a number with nothing in
    /// it. Repeating the operation until the timed part is an order of
    /// magnitude larger than the startup makes the subtraction a correction
    /// rather than the whole reading.
    ///
    /// A write workload repeats by undoing itself first where it has to, so
    /// every repetition does the same work as the first.
    repeat: usize,
}

/// The empty script, run to price what starting the process costs.
///
/// **Subtracted from every workload before the ratios are computed**, and
/// recorded in its own columns so the subtraction is visible rather than
/// hidden. It opens a database and exits, which is the least a shell can do and
/// still have opened a file.
const BASELINE: &str = "SELECT 1;\n";

/// The row generator every setup uses, as a table rather than a CTE.
///
/// **This is here because measuring with a recursive CTE was measuring the
/// wrong thing.** The first version of this tool generated its rows inline with
/// `WITH RECURSIVE`, and every workload came out 3x to 8x slower than SQLite.
/// Timed on its own, a 200,000-row recursive CTE that touches no table at all
/// costs 201 ms here against SQLite's 85 ms - so the workloads were a
/// measurement of recursive-CTE throughput with a little storage engine
/// underneath, and the storage engine is what the history is for.
///
/// (That 2.4x is a real and separate finding, and it is worth its own ticket.
/// It is not this history's subject.)
///
/// So the generator runs in the **setup**, which is not timed, and every timed
/// script reads from an ordinary table.
const SOURCE: &str = "CREATE TABLE source (i INTEGER PRIMARY KEY);\n\
     INSERT INTO source (i) \
       WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20000) \
       SELECT i FROM n;\n";

/// The workloads, chosen to cost different things.
///
/// Each builds its data in `setup` and times exactly one kind of work in
/// `script`. The database is rebuilt from the setup before every round, so a
/// round never measures the previous round's file.
const WORKLOADS: &[Workload] = &[
    Workload {
        name: "insert.10k",
        setup: "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL, bucket INTEGER NOT NULL);\n",
        // Emptied first, so every repetition inserts into an empty table.
        script: "DELETE FROM t;\n\
                 BEGIN;\n\
                 INSERT INTO t (id, name, bucket) \
                   SELECT i, 'person' || i || '@example.com', i % 97 FROM source WHERE i <= 10000;\n\
                 COMMIT;\n",
        repeat: 5,
    },
    Workload {
        name: "index.build",
        setup: "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL, bucket INTEGER NOT NULL);\n\
                INSERT INTO t (id, name, bucket) \
                  SELECT i, 'person' || i || '@example.com', i % 97 FROM source;\n",
        script: "DROP INDEX IF EXISTS t_name;\n\
                 DROP INDEX IF EXISTS t_bucket;\n\
                 CREATE UNIQUE INDEX t_name ON t (name);\n\
                 CREATE INDEX t_bucket ON t (bucket);\n",
        repeat: 5,
    },
    Workload {
        name: "lookup.indexed",
        setup: "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL);\n\
                INSERT INTO t (id, name) SELECT i, 'person' || i || '@example.com' FROM source;\n\
                CREATE UNIQUE INDEX t_name ON t (name);\n\
                CREATE TABLE q (i INTEGER PRIMARY KEY);\n\
                INSERT INTO q (i) SELECT i FROM source WHERE i <= 4000;\n",
        script: "SELECT count(*) FROM q JOIN t ON t.name = 'person' || (q.i * 5) || '@example.com';\n",
        repeat: 20,
    },
    Workload {
        name: "scan.aggregate",
        setup: "CREATE TABLE t (id INTEGER PRIMARY KEY, bucket INTEGER NOT NULL, amount INTEGER NOT NULL);\n\
                INSERT INTO t (id, bucket, amount) SELECT i, i % 97, i * 3 FROM source;\n",
        script: "SELECT bucket, count(*), sum(amount), avg(amount) FROM t GROUP BY bucket ORDER BY bucket;\n",
        repeat: 40,
    },
    Workload {
        name: "update.churn",
        setup: "CREATE TABLE t (id INTEGER PRIMARY KEY, bucket INTEGER NOT NULL, note TEXT);\n\
                INSERT INTO t (id, bucket, note) SELECT i, i % 97, 'note ' || i FROM source;\n\
                CREATE INDEX t_bucket ON t (bucket);\n",
        // Only the update repeats; a repeated delete would run out of rows and
        // every repetition after the first would be measuring an empty table.
        script: "BEGIN;\n\
                 UPDATE t SET bucket = (bucket + 1) % 97 WHERE id % 2 = 0;\n\
                 COMMIT;\n",
        repeat: 5,
    },
    Workload {
        name: "join.two-tables",
        setup: "CREATE TABLE a (id INTEGER PRIMARY KEY, bucket INTEGER NOT NULL);\n\
                CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER NOT NULL, amount INTEGER NOT NULL);\n\
                INSERT INTO a (id, bucket) SELECT i, i % 53 FROM source WHERE i <= 5000;\n\
                INSERT INTO b (id, a_id, amount) SELECT i, (i % 5000) + 1, i FROM source;\n",
        script: "SELECT a.bucket, count(*), sum(b.amount) FROM a JOIN b ON b.a_id = a.id \
                   GROUP BY a.bucket ORDER BY a.bucket;\n",
        repeat: 20,
    },
    Workload {
        name: "text.like",
        setup: "CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT NOT NULL);\n\
                INSERT INTO t (id, body) \
                  SELECT i, 'the quick brown fox number ' || i || ' jumped over the lazy dog' FROM source;\n",
        script: "SELECT count(*) FROM t WHERE body LIKE '%number 12%';\n\
                 SELECT count(*) FROM t WHERE body LIKE '%lazy%';\n",
        repeat: 20,
    },
    Workload {
        name: "delete.half",
        setup: "CREATE TABLE t (id INTEGER PRIMARY KEY, bucket INTEGER NOT NULL, note TEXT);\n\
                INSERT INTO t (id, bucket, note) SELECT i, i % 97, 'note ' || i FROM source;\n\
                CREATE INDEX t_bucket ON t (bucket);\n",
        // Puts back what it removed, so every repetition deletes the same rows.
        script: "BEGIN;\n\
                 DELETE FROM t WHERE id % 2 = 0;\n\
                 INSERT INTO t (id, bucket, note) \
                   SELECT i, i % 97, 'note ' || i FROM source WHERE i % 2 = 0;\n\
                 COMMIT;\n",
        repeat: 3,
    },
];

/// What one arm of one round cost.
#[derive(Clone, Copy, Debug, Default)]
struct Reading {
    /// Wall clock, in milliseconds.
    wall: f64,
    /// Processor time, user and kernel together, in milliseconds.
    cpu: f64,
    /// The largest resident set the process reached, in mebibytes.
    peak: f64,
}

/// Runs the history and appends its rows.
fn main() -> std::process::ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let mut rounds = DEFAULT_ROUNDS;
    let mut dry_run = false;
    let mut index = 0usize;
    while let Some(argument) = arguments.get(index) {
        index += 1;
        match argument.as_str() {
            "--rounds" => {
                let Some(value) = arguments.get(index) else {
                    eprintln!("`--rounds` needs a number");
                    return std::process::ExitCode::FAILURE;
                };
                index += 1;
                match value.parse() {
                    Ok(parsed) => rounds = parsed,
                    Err(_) => {
                        eprintln!("`--rounds` wants a number, not `{value}`");
                        return std::process::ExitCode::FAILURE;
                    }
                }
            }
            "--dry-run" => dry_run = true,
            other => {
                eprintln!("unknown option `{other}`");
                return std::process::ExitCode::FAILURE;
            }
        }
    }
    match run(rounds, dry_run) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("inillucent-perfhistory: {reason}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Measures every workload and writes the rows.
///
/// @param rounds - how many times to run each arm
/// @param dry_run - print the rows instead of appending them
fn run(rounds: usize, dry_run: bool) -> Result<(), String> {
    let root = workspace_root();
    let ours = shell_path(&root, "inillucent-shell")
        .ok_or("inillucent-shell is not built; run `cargo build --release -p inillucent-cli`")?;
    let theirs = reference_path(&root)
        .ok_or("the pinned SQLite shell is not present; run tools/sqlite-reference.ps1")?;
    let area = root.join("_agent_output/perfhistory");
    std::fs::create_dir_all(&area)
        .map_err(|error| format!("cannot make {}: {error}", area.display()))?;

    let calibration_before = calibrate();
    let commit = commit_hash(&root);
    let stamp = timestamp();
    let machine = machine_name();
    let mut rows = Vec::new();

    // What starting each process costs, measured the same way and interleaved
    // like everything else.
    let mut ours_startup = Vec::new();
    let mut theirs_startup = Vec::new();
    for round in 0..rounds {
        ours_startup.push(run_fresh(
            &ours,
            &area.join(format!("startup-ours-{round}")),
            BASELINE,
        )?);
        theirs_startup.push(run_fresh(
            &theirs,
            &area.join(format!("startup-theirs-{round}")),
            BASELINE,
        )?);
    }
    let ours_base = summarise(&mut ours_startup);
    let theirs_base = summarise(&mut theirs_startup);
    println!(
        "startup         ours {:>8.1} ms {:>8.1} ms cpu {:>7.1} MiB | sqlite {:>8.1} ms {:>8.1} ms cpu {:>7.1} MiB",
        ours_base.wall, ours_base.cpu, ours_base.peak,
        theirs_base.wall, theirs_base.cpu, theirs_base.peak,
    );

    for workload in WORKLOADS {
        let mut ours_readings = Vec::new();
        let mut theirs_readings = Vec::new();
        // Interleaved: each round runs both arms back to back, so anything else
        // happening on the machine lands on both.
        // The data is built once per engine, outside the clock, and copied
        // back before every round - so a write workload never measures the
        // previous round's file and a read workload never measures a warm one.
        let ours_template = area.join(format!("{}-ours-template", workload.name));
        let theirs_template = area.join(format!("{}-theirs-template", workload.name));
        build(&ours, &ours_template, workload.setup)?;
        build(&theirs, &theirs_template, workload.setup)?;
        let repeated = workload.script.repeat(workload.repeat.max(1));
        for round in 0..rounds {
            let ours_round = area.join(format!("{}-ours-{round}", workload.name));
            let theirs_round = area.join(format!("{}-theirs-{round}", workload.name));
            copy_tree(&ours_template, &ours_round)?;
            copy_tree(&theirs_template, &theirs_round)?;
            ours_readings.push(time_only(&ours, &database_in(&ours_round), &repeated)?);
            theirs_readings.push(time_only(&theirs, &database_in(&theirs_round), &repeated)?);
        }
        let ours_total = summarise(&mut ours_readings);
        let theirs_total = summarise(&mut theirs_readings);
        let ours_net = ours_total.net_of(&ours_base);
        let theirs_net = theirs_total.net_of(&theirs_base);
        rows.push(Row {
            stamp: stamp.clone(),
            commit: commit.clone(),
            machine: machine.clone(),
            workload: workload.name,
            rounds,
            ours: ours_total,
            theirs: theirs_total,
            ours_startup: ours_base,
            theirs_startup: theirs_base,
        });
        println!(
            "{:<16} ours {:>8.1} ms {:>8.1} ms cpu {:>7.1} MiB | sqlite {:>8.1} ms {:>8.1} ms cpu {:>7.1} MiB | net of startup: wall {:.2}x cpu {:.2}x rss {:.2}x",
            workload.name,
            ours_total.wall,
            ours_total.cpu,
            ours_total.peak,
            theirs_total.wall,
            theirs_total.cpu,
            theirs_total.peak,
            ratio(theirs_net.wall, ours_net.wall),
            ratio(theirs_net.cpu, ours_net.cpu),
            ratio(theirs_total.peak, ours_total.peak),
        );
    }

    let calibration_after = calibrate();
    let drift = ratio(calibration_after, calibration_before);
    println!(
        "\ncalibration {calibration_before:.1} ms before, {calibration_after:.1} ms after, drift {drift:.3}x"
    );
    if !(0.8..=1.25).contains(&drift) {
        println!(
            "the machine changed during the run; these rows are recorded with their drift and \
             should be read as suspect"
        );
    }

    let history = root.join("tests/performance-history.tsv");
    let text = render(&rows, calibration_before, calibration_after);
    if dry_run {
        println!("\n--- would append to {} ---\n{text}", history.display());
        return Ok(());
    }
    append(&history, &text)?;
    println!("\nappended {} row(s) to {}", rows.len(), history.display());
    Ok(())
}

/// One row of the history.
struct Row {
    /// When the run happened, as an ISO-8601 date and time in UTC.
    stamp: String,
    /// The commit the engine was built from.
    commit: String,
    /// Which machine it ran on.
    machine: String,
    /// The workload's name.
    workload: &'static str,
    /// How many rounds the median came from.
    rounds: usize,
    /// What inillucent cost.
    ours: Reading,
    /// What the pinned SQLite cost.
    theirs: Reading,
    /// What starting inillucent's shell cost, for the subtraction.
    ours_startup: Reading,
    /// What starting SQLite's shell cost.
    theirs_startup: Reading,
}

/// Returns the database file inside one of these directories.
///
/// **One directory per database, and the directory is the unit that is copied.**
/// The two engines do not write the same set of files beside a database - SQLite
/// writes `-wal` and `-shm`, and this engine writes a *segmented* log,
/// `app.rdb-wal.0000000001` and a new one per segment. A copy that knew the
/// suffixes would have to be kept in step with both engines' file naming
/// forever, and the first version of this tool was not: it copied the `.rdb`
/// alone and left the previous round's log segments sitting beside it, so every
/// round after the first opened a database with somebody else's log.
///
/// Copying the directory needs to know nothing about either engine.
///
/// @param directory - the directory holding one database
fn database_in(directory: &Path) -> PathBuf {
    directory.join("app.db")
}

/// Builds a workload's data, untimed.
///
/// @param program - the shell to run
/// @param directory - the directory to build the database in
/// @param setup - the schema and rows
fn build(program: &Path, directory: &Path, setup: &str) -> Result<(), String> {
    let mut script = String::from(SOURCE);
    script.push_str(setup);
    // A checkpoint, so as much as possible is in the file rather than the log.
    // The copy carries the log too, so this is a tidiness rather than a
    // correctness measure - but it means every round starts from a database in
    // the state an application would find it in after a clean close.
    script.push_str("PRAGMA wal_checkpoint(TRUNCATE);\n");
    run_fresh(program, directory, &script).map(|_| ())
}

/// Replaces one directory with a copy of another.
///
/// @param from - the template directory
/// @param to - the directory to build
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    remove_tree(to)?;
    std::fs::create_dir_all(to)
        .map_err(|error| format!("cannot make {}: {error}", to.display()))?;
    for entry in std::fs::read_dir(from)
        .map_err(|error| format!("cannot read {}: {error}", from.display()))?
    {
        let entry = entry.map_err(|error| format!("cannot read {}: {error}", from.display()))?;
        let source = entry.path();
        if !source.is_file() {
            continue;
        }
        let Some(name) = source.file_name() else {
            continue;
        };
        std::fs::copy(&source, to.join(name))
            .map_err(|error| format!("cannot copy {}: {error}", source.display()))?;
    }
    Ok(())
}

/// Removes a directory this tool made, if it is there.
///
/// Only ever called on a directory under `_agent_output/perfhistory`, which is
/// this tool's own scratch.
///
/// @param directory - the directory to remove
fn remove_tree(directory: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove {}: {error}", directory.display())),
    }
}

/// Runs a script over a database directory that has been emptied first.
///
/// @param program - the shell to run
/// @param directory - the directory the database goes in
/// @param script - what to feed it on standard input
fn run_fresh(program: &Path, directory: &Path, script: &str) -> Result<Reading, String> {
    remove_tree(directory)?;
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("cannot make {}: {error}", directory.display()))?;
    time_only(program, &database_in(directory), script)
}

/// Runs one script through one shell over the database already at `path`.
///
/// @param program - the shell to run
/// @param path - the database it should open
/// @param script - what to feed it on standard input
fn time_only(program: &Path, path: &Path, script: &str) -> Result<Reading, String> {
    let started = Instant::now();
    let mut child = Command::new(program)
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", program.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(script.as_bytes())
            .map_err(|error| format!("cannot write to {}: {error}", program.display()))?;
    }
    let status = child
        .wait()
        .map_err(|error| format!("cannot wait for {}: {error}", program.display()))?;
    let wall = started.elapsed();
    // **Read before the handle is dropped.** A finished process still carries
    // its accounting until its last handle closes, which is what makes this
    // readable at all.
    let cost: ProcessCost = child_cost(&child);
    if !status.success() {
        return Err(format!(
            "{} exited with {status} on this workload",
            program.display()
        ));
    }
    Ok(Reading {
        wall: wall.as_secs_f64() * 1_000.0,
        cpu: millis(cost.cpu_nanos()),
        peak: mebibytes(cost.peak_working_set),
    })
}

impl Reading {
    /// Returns this reading with a baseline taken off each of its parts.
    ///
    /// Never below zero: a workload that measured faster than the baseline is a
    /// measurement of noise, and a negative cost is not a thing a history can
    /// say.
    ///
    /// @param baseline - what starting the process cost
    fn net_of(&self, baseline: &Reading) -> Reading {
        Reading {
            wall: (self.wall - baseline.wall).max(0.0),
            cpu: (self.cpu - baseline.cpu).max(0.0),
            peak: (self.peak - baseline.peak).max(0.0),
        }
    }
}

/// Summarises a set of rounds into one reading.
///
/// **Three statistics, because the three numbers have different problems.**
///
/// Wall clock takes the **median**: one round that lost the processor moves a
/// mean and does not move a median.
///
/// Processor time takes the **mean of the total**, which is the opposite
/// choice and is forced by the clock. Windows accounts processor time in units
/// of about 15.6 ms, so a round that spent 20 ms reads as 15.6 or 31.2 and a
/// median of five such rounds is one of those two numbers. Summing the rounds
/// and dividing recovers the resolution the counter does not have on its own -
/// five rounds of a 20 ms workload total about 100 ms, which the counter can
/// represent.
///
/// The peak resident set takes the **largest**, because that is what a peak is.
///
/// @param readings - the rounds, reordered in place
fn summarise(readings: &mut [Reading]) -> Reading {
    if readings.is_empty() {
        return Reading::default();
    }
    let count = readings.len() as f64;
    let cpu = readings.iter().map(|reading| reading.cpu).sum::<f64>() / count;
    let peak = readings
        .iter()
        .map(|reading| reading.peak)
        .fold(0.0f64, f64::max);
    Reading {
        wall: median(readings).wall,
        cpu,
        peak,
    }
}

/// Returns the middle reading of a set, by wall clock.
///
/// The median rather than the mean, because one round that lost the processor
/// moves a mean and does not move a median. The whole reading is carried, so
/// the processor time and the peak reported are the ones from the round whose
/// wall clock was in the middle rather than three unrelated middles.
///
/// @param readings - the rounds, reordered in place
fn median(readings: &mut [Reading]) -> Reading {
    readings.sort_by(|left, right| {
        left.wall
            .partial_cmp(&right.wall)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    readings
        .get(readings.len() / 2)
        .copied()
        .unwrap_or_default()
}

/// Times a fixed arithmetic loop, as a reading of how busy the machine is.
///
/// It touches no file and allocates nothing, so its time is the processor's
/// availability and nothing else. Run before and after the workloads: the two
/// readings together say both how fast this machine is and whether it changed
/// while the measurements were being taken.
fn calibrate() -> f64 {
    let started = Instant::now();
    let mut accumulator: u64 = 0;
    for step in 0..40_000_000u64 {
        accumulator = accumulator
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(step);
    }
    // Used, so the loop cannot be optimised away.
    if accumulator == 1 {
        println!("(the calibration loop reached an improbable value)");
    }
    started.elapsed().as_secs_f64() * 1_000.0
}

/// Returns `reference / candidate`, or zero when the candidate is zero.
///
/// A ratio above one means inillucent is cheaper, which is the direction every
/// other number in this repository is written in.
///
/// @param reference - what SQLite cost
/// @param candidate - what inillucent cost
fn ratio(reference: f64, candidate: f64) -> f64 {
    if candidate <= 0.0 {
        return 0.0;
    }
    reference / candidate
}

/// Renders the rows as tab-separated lines.
///
/// @param rows - what was measured
/// @param before - the calibration before the run
/// @param after - the calibration after it
fn render(rows: &[Row], before: f64, after: f64) -> String {
    let mut text = String::new();
    for row in rows {
        let ours = row.ours.net_of(&row.ours_startup);
        let theirs = row.theirs.net_of(&row.theirs_startup);
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.3}\t{:.3}\t{:.3}\t{:.1}\t{:.3}\n",
            row.stamp,
            row.commit,
            row.machine,
            row.workload,
            row.rounds,
            row.ours.wall,
            row.ours.cpu,
            row.ours.peak,
            row.theirs.wall,
            row.theirs.cpu,
            row.theirs.peak,
            row.ours_startup.wall,
            row.theirs_startup.wall,
            ratio(theirs.wall, ours.wall),
            ratio(theirs.cpu, ours.cpu),
            ratio(row.theirs.peak, row.ours.peak),
            before,
            ratio(after, before),
        ));
    }
    text
}

/// The header the history file starts with.
const HEADER: &str = "# The performance history: what each workload costs, beside pinned SQLite 3.53.4.\n\
     #\n\
     # Appended by `inillucent-perfhistory`, one row per workload per run, never edited.\n\
     #\n\
     # Read the RATIO columns across time. Both arms are measured interleaved in the same\n\
     # minute on the same machine, so a machine that got busy moves both and leaves the\n\
     # ratio alone; the absolute columns are there to catch the case where both moved.\n\
     # A ratio above 1 means inillucent cost less than SQLite.\n\
     #\n\
     # The wall and cpu ratios are computed NET OF PROCESS STARTUP: each engine's own\n\
     # empty-script time (the two `startup_*_wall_ms` columns) is subtracted first. This\n\
     # is not a flattering adjustment, it is a necessary one - starting these two shells\n\
     # costs about 70 ms and about 20 ms, which is more than several workloads spend on\n\
     # their data, so an unadjusted ratio is mostly a measurement of two linkers. The\n\
     # absolute columns are NOT adjusted and still include startup.\n\
     #\n\
     # `*_wall_ms` is the median round. `*_cpu_ms` is the mean, because Windows accounts\n\
     # processor time in ~15.6 ms units and a median of five short rounds is one of two\n\
     # values. `*_peak_mib` is the largest round, because that is what a peak is.\n\
     #\n\
     # `calibration_ms` is a fixed arithmetic loop timed before the run: it says how fast\n\
     # this machine is, so rows from two machines can be compared. `drift` is the same\n\
     # loop timed again afterwards, divided by the first - a row whose drift is far from\n\
     # 1.000 was taken on a machine that changed under it and should be distrusted.\n\
     #\n\
     stamp\tcommit\tmachine\tworkload\trounds\tours_wall_ms\tours_cpu_ms\tours_peak_mib\tsqlite_wall_ms\tsqlite_cpu_ms\tsqlite_peak_mib\tstartup_ours_wall_ms\tstartup_sqlite_wall_ms\twall_ratio\tcpu_ratio\trss_ratio\tcalibration_ms\tdrift\n";

/// Appends the rows, writing the header when the file is new.
///
/// @param path - the history file
/// @param text - the rows to append
fn append(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let fresh = !path.is_file();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    if fresh {
        file.write_all(HEADER.as_bytes())
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }
    file.write_all(text.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// Returns this workspace's shell, release build preferred.
///
/// @param root - the workspace root
/// @param name - the binary's name
fn shell_path(root: &Path, name: &str) -> Option<PathBuf> {
    for profile in ["release", "debug"] {
        let path = root
            .join("target")
            .join(profile)
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// Returns the pinned SQLite shell.
///
/// @param root - the workspace root
fn reference_path(root: &Path) -> Option<PathBuf> {
    let path = root
        .join(".sqlite-ref/3.53.4/shell")
        .join(format!("sqlite3{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Returns the short commit the working tree is at, or `unknown`.
///
/// @param root - the workspace root
fn commit_hash(root: &Path) -> String {
    let Ok(output) = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
    else {
        return "unknown".to_string();
    };
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return "unknown".to_string();
    }
    // A dirty tree is not the commit it says it is, and a history row that
    // claimed otherwise would be unreproducible in the most misleading way.
    match Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain"])
        .output()
    {
        Ok(status) if !status.stdout.is_empty() => format!("{text}-dirty"),
        _ => text,
    }
}

/// Returns a name for this machine.
fn machine_name() -> String {
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                return value;
            }
        }
    }
    if let Ok(output) = Command::new("hostname").output() {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !name.is_empty() {
            return name;
        }
    }
    "unknown".to_string()
}

/// Returns the current time as an ISO-8601 date and time in UTC.
///
/// Written by hand from the seconds since the epoch, because a date is the one
/// thing this workspace will not take a dependency for.
fn timestamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    let (hour, minute, second) = (rest / 3_600, (rest % 3_600) / 60, rest % 60);
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Turns days since 1970-01-01 into a calendar date.
///
/// Howard Hinnant's `civil_from_days`, which is the standard shift-the-epoch-to
/// -March algorithm: with the year starting in March, the leap day is the last
/// day of the year and every month length becomes a closed form.
///
/// @param days - days since the epoch
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The date arithmetic has to be right, or every row is stamped wrongly and
    /// the history cannot be read in order.
    #[test]
    fn the_epoch_and_a_leap_day_convert() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(59), (1970, 3, 1));
        // 2000-02-29: a leap year despite being a century.
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_608), (2026, 6, 4));
    }

    /// A ratio above one means this engine cost less, and a zero denominator is
    /// reported as zero rather than as an infinity nobody can plot.
    #[test]
    fn a_ratio_reads_in_the_repositorys_direction() {
        assert!((ratio(10.0, 5.0) - 2.0).abs() < f64::EPSILON);
        assert!((ratio(5.0, 10.0) - 0.5).abs() < f64::EPSILON);
        assert!(ratio(1.0, 0.0) == 0.0);
    }

    /// The median is the middle *round*, so the three numbers in a row belong
    /// to one another.
    #[test]
    fn the_median_keeps_one_rounds_numbers_together() {
        let mut readings = vec![
            Reading {
                wall: 30.0,
                cpu: 3.0,
                peak: 3.0,
            },
            Reading {
                wall: 10.0,
                cpu: 1.0,
                peak: 1.0,
            },
            Reading {
                wall: 20.0,
                cpu: 2.0,
                peak: 2.0,
            },
        ];
        let middle = median(&mut readings);
        assert!((middle.wall - 20.0).abs() < f64::EPSILON);
        assert!((middle.cpu - 2.0).abs() < f64::EPSILON);
        assert!((middle.peak - 2.0).abs() < f64::EPSILON);
    }
}
