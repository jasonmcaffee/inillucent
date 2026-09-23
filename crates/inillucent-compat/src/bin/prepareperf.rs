//! `open.prepare` on the shipped engine, with the plan cache and without it.
//!
//! Invariant: both arms prepare the *same* SQL against the *same* rows and step
//! the resulting statement, and each arm's answer is checked before its time is
//! read. The only difference between them is one lever.
//!
//! ## What this measures
//!
//! The arms are the shipped engine and the shipped engine with
//! `Levers::PLAN_CACHE` switched off, and the reference is `sqlite-bench`
//! running the same plan file - which is exactly the `open.prepare` family the
//! scorecard measures, extracted so it can be run without a full scorecard.
//!
//! **This used to measure the old, pre-rearchitecture engine** (`inillucent-session`,
//! reached through `inillucent-legacy`), on the reasoning that the plan cache
//! landed in the new engine's prepare path and needed proving "independently of
//! the new storage" before that storage existed to measure it against. The old
//! engine is deleted now and the new one is what ships, so this measures
//! `inillucent-engine` directly - the same plan cache, `Levers::PLAN_CACHE`, moved
//! down into `inillucent_sql::plan` where both engines could read it, and now
//! only one does.
//!
//! The family is `prepare_each: true`: the timed loop re-prepares the same text
//! every iteration, which is the shape an application that does not hold
//! statements has, and the shape a plan cache is for. SQLite has no equivalent
//! inside the library, and the fairness section of the scorecard report is where
//! that is declared - not here, and not silently.
//!
//! ## One fixture, two files, because no one file satisfies both arms
//!
//! **This program could not be run at all until task-2041.** It took one path
//! and handed it to both arms: `Database::open` on a SQLite `.db` reports that
//! neither meta page is readable, which is correct - this engine writes its own
//! format - and `sqlite-bench` on a `.rdb` cannot read it either. A `.db`
//! failed the first arm and a `.rdb` failed the second, so no argument worked,
//! and nothing in `tests/selection.toml` ran the binary to find out.
//!
//! It now does what `fullgate` does: it takes the SQLite fixture
//! `tools/build-gate-fixtures.sh` builds, **imports one copy** into a scratch
//! `.rdb` for the native arms, and hands the original to `sqlite-bench`. The
//! import happens once rather than once a round, because every workload in this
//! family reads and none of them writes, so the file the native arms open is
//! the same file at every round.
//!
//! The row count comes off that fixture rather than being written in here, and
//! goes into both the plan file `sqlite-bench` reads and the parameter this
//! side binds. That is what lets the program run against the small fixture as
//! well as the medium one: the scatter formula is keyed on the row count, so a
//! hard-coded 100,000 against a 5,000-row fixture would have both arms reading
//! ids that are not there.
//!
//! ## These numbers are not `fullgate`'s, and the breakdown says why
//!
//! This program's inillucent arm goes through the shipped API - `Database::open`,
//! `session()`, `prepare`, `step` - which is what an application binds to.
//! `fullgate` drives the engine's own `plan`, `prepare` and `pipeline` calls
//! instead. On the medium fixture, five rounds, release build, 2026-09-20, the
//! two disagree by about **85 times** on the same workload: `prepare.trivial`
//! is 7.6 ms a round through `fullgate` and 649 ms a round here.
//!
//! The breakdown table below is where that difference is visible rather than
//! asserted. On the same run `prepare.trivial` cost **64 times** as much per
//! prepare-and-step outside a transaction as inside one - 158,651 ns against
//! 2,493 ns - on the same connection over the same file, with nothing differing
//! between the two but a `BEGIN`. Everything in that gap is what a statement's
//! implicit read transaction costs.
//!
//! **That 64 times was a defect, and task-2046 fixed it.** The staleness check
//! a statement makes on its way in read the meta record twice, a whole page per
//! slot per read, allocated and checksummed, to compare a record 116 bytes
//! long; and the Windows lock release unlocked two byte ranges a handle at
//! SHARED does not hold. On an idle box, 2026-09-21, two runs of this program
//! read `prepare.trivial` at 14,691 ns and 8,549 ns outside a transaction
//! against 869 ns and 775 ns inside one - **11 to 17 times**. The two runs
//! disagree by 72% on the outside figure because the first process to run after
//! the box has been idle pays for a cold file cache on a 16 MB fixture, which
//! is why the claim is a range rather than a number.
//!
//! What is left is seven system calls: three to take the SHARED lock, one to
//! release it, two reads of 116 bytes, and one `file_size` on the log segment.
//! That is `locking_mode = normal` costing what it costs on this platform
//! rather than anything further to remove.
//!
//! The numbers above are ratios on purpose: the 2026-09-20 pair was taken with
//! other agents building, which moves an absolute nanosecond and leaves a ratio
//! where it was. The 2026-09-21 pair was taken with the box measured at 6% and
//! 2%, which is what makes those two absolute.
//!
//! So a reader comparing this report's family ratio against the scorecard's
//! `open.prepare` is comparing two measurements of different things. This one
//! is what the shipped API costs.
//!
//! Usage: inillucent-prepareperf `<sqlite fixture>` `[rounds]` `[--repeat n]`

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use inillucent_compat::perf::run_sqlite;
use inillucent_compat::perf::{bind_value, family_interval, median, Bind, Digest, Paired, Sample};
use inillucent_compat::workspace_root;
use inillucent_engine::connect::{Connection, Database, Statement};
use inillucent_engine::DEFAULT_FRAMES;
use inillucent_sql::plan::Levers;
use inillucent_tree::datum::OwnedDatum;

/// The seed the bootstrap uses, fixed so a report is reproducible.
const SEED: u64 = 17_900_001;

/// The `open.prepare` workloads, byte for byte as `scorecard.rs` generates them.
/// There are two of them in `scorecard.rs`, and inventing a third would make
/// this a different family than the one the contract weights.
const WORKLOADS: [(&str, &str, bool); 2] = [
    ("prepare.trivial", "SELECT 1", false),
    (
        "prepare.point",
        "SELECT label FROM main_table WHERE id = ?1",
        true,
    ),
];

/// How many times each workload prepares inside one timed round, by default.
///
/// `repeats_for("medium").0` in `scorecard.rs`, which is what the
/// `open.prepare` family uses. `--repeat` moves it, which is how a test can run
/// the program to completion in a second; the published numbers are the
/// default.
const DEFAULT_REPEAT: u32 = 4_000;

/// How many paired rounds a run takes when none is asked for.
const DEFAULT_ROUNDS: u32 = 30;

/// What one run was asked to do.
struct Options {
    /// The SQLite fixture both arms are built from.
    fixture: PathBuf,
    /// How many paired rounds to take.
    rounds: u32,
    /// How many prepares each workload does inside one round.
    repeat: u32,
}

impl Options {
    /// Reads the command line.
    ///
    /// The fixture and the round count are positional, which is what the
    /// scorecard-era callers pass; `--repeat` is the one flag.
    ///
    /// @param arguments - the command line, without the program name
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Options, String> {
        let mut positional: Vec<String> = Vec::new();
        let mut repeat = DEFAULT_REPEAT;
        let mut rounds: Option<u32> = None;
        let mut arguments = arguments;
        while let Some(argument) = arguments.next() {
            let flag = matches!(argument.as_str(), "--repeat" | "--rounds");
            if !flag {
                positional.push(argument);
                continue;
            }
            let Some(value) = arguments.next() else {
                return Err(format!("{argument} wants a number after it"));
            };
            let parsed: u32 = value
                .parse()
                .map_err(|_| format!("{argument} {value} is not a number"))?;
            if argument == "--repeat" {
                repeat = parsed;
            } else {
                rounds = Some(parsed);
            }
        }
        let Some(fixture) = positional.first() else {
            return Err(
                "usage: inillucent-prepareperf <sqlite fixture> [rounds] [--repeat n] \
                 [--cores performance|efficiency|any]"
                    .to_string(),
            );
        };
        let rounds = rounds
            .or_else(|| positional.get(1).and_then(|value| value.parse().ok()))
            .unwrap_or(DEFAULT_ROUNDS);
        if rounds == 0 || repeat == 0 {
            return Err("a run of no rounds, or of no repeats, measures nothing".to_string());
        }
        Ok(Options {
            fixture: PathBuf::from(fixture),
            rounds,
            repeat,
        })
    }
}

/// Everything a run needs that it could fail to get, gathered before it starts.
///
/// **Separate from `run` so that "could not start" and "ran and missed" are
/// different exit codes**, which is the distinction the gate programs in this
/// directory already draw and `tests/gates_fail_closed.rs` asserts on: 2 means
/// nothing was measured, 0 and 1 mean it ran.
struct Preflight {
    /// The pinned benchmark driver, for the SQLite arm.
    bench: PathBuf,
    /// The directory this run's own files live in.
    scratch: PathBuf,
    /// The imported copy of the fixture, for the native arms.
    imported: PathBuf,
    /// The plan file both the reference and this side are described by.
    plan: PathBuf,
    /// How many rows `main_table` holds, which both arms bind against.
    rows: u32,
}

fn main() -> ExitCode {
    let mut arguments: Vec<String> = std::env::args().skip(1).collect();
    // **Pinned before anything is timed, and the mask printed (task-2085).**
    // `run_sqlite` checks that `sqlite-bench` inherited the same mask.
    if let Err(reason) = inillucent_compat::affinity::pin_from_arguments(&mut arguments) {
        eprintln!("{reason}");
        return ExitCode::from(2);
    }
    let options = match Options::parse(arguments.into_iter()) {
        Ok(options) => options,
        Err(reason) => {
            eprintln!("{reason}");
            return ExitCode::from(2);
        }
    };
    let preflight = match preflight(&options) {
        Ok(preflight) => preflight,
        Err(reason) => {
            eprintln!("{reason}");
            return ExitCode::from(2);
        }
    };
    match run(&options, &preflight) {
        Ok(()) => {
            // The import and the plan file are this run's own, and a medium
            // fixture's import is the size of the fixture.
            let _ = std::fs::remove_dir_all(&preflight.scratch);
            ExitCode::SUCCESS
        }
        Err(reason) => {
            eprintln!("{reason}");
            eprintln!(
                "  the import this run was reading is at {}",
                preflight.imported.display()
            );
            ExitCode::FAILURE
        }
    }
}

/// Gathers what a run needs, and says what is missing when it cannot.
///
/// @param options - what the command line asked for
fn preflight(options: &Options) -> Result<Preflight, String> {
    if !options.fixture.is_file() {
        return Err(format!(
            "the SQLite fixture is not there: {}\n  build one with tools/build-gate-fixtures.sh",
            options.fixture.display()
        ));
    }
    let bench = sqlite_bench().ok_or_else(|| {
        "sqlite-bench is not built; run tools/sqlite-reference.sh (Linux, macOS) or \
         tools/sqlite-reference.ps1 (Windows)"
            .to_string()
    })?;
    let scratch = workspace_root()
        .join("_agent_output/prepareperf")
        .join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).map_err(|error| format!("scratch: {error}"))?;
    // A preflight that does not finish takes its own directory with it. The
    // import is kept for reading only when a *run* fails, which is when there
    // is something in it worth reading; a failure here leaves an empty
    // directory named after a process that has gone, and one a run would
    // accumulate.
    let gathered = gather(options, bench, &scratch);
    if gathered.is_err() {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    gathered
}

/// Imports the fixture, counts its rows and writes the plan file.
///
/// Split out of `preflight` so that one `?` can undo the directory all three
/// steps write into.
///
/// @param options - what the command line asked for
/// @param bench - the benchmark driver already found
/// @param scratch - the directory this run owns
fn gather(options: &Options, bench: PathBuf, scratch: &Path) -> Result<Preflight, String> {
    let imported = import_for_the_native_arm(&options.fixture, scratch)?;
    let rows = main_table_rows(&imported)?;
    let plan = scratch.join("open.prepare.plan");
    std::fs::write(&plan, plan_file(rows, options.repeat))
        .map_err(|error| format!("plan: {error}"))?;
    Ok(Preflight {
        bench,
        scratch: scratch.to_path_buf(),
        imported,
        plan,
        rows,
    })
}

/// Rebuilds the SQLite fixture as a database this engine reads.
///
/// The source is never written to: `import_into` reads it and writes a separate
/// file, which is what lets the SQLite arm keep reading the original while the
/// native arms read the import.
///
/// The pool is `DEFAULT_FRAMES`, which is what `Database::open` gives every
/// round afterwards - so the import and the runs that read it hold the same
/// number of pages by construction rather than by two constants agreeing.
///
/// @param fixture - the SQLite database the fixture builder wrote
/// @param scratch - the directory this run owns
fn import_for_the_native_arm(fixture: &Path, scratch: &Path) -> Result<PathBuf, String> {
    let target = scratch.join("ours.rdb");
    let started = Instant::now();
    Database::import_into(fixture.to_path_buf(), target.clone(), DEFAULT_FRAMES).map_err(
        |error| {
            format!(
                "importing {} failed: {}\n  this program takes the SQLite fixture, not a \
                 database this engine already wrote",
                fixture.display(),
                error.message()
            )
        },
    )?;
    println!(
        "imported {} in {:.1}s",
        fixture.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(target)
}

/// Returns how many rows the fixture's `main_table` holds.
///
/// **The scatter formula is keyed on this number and so is `sqlite-bench`'s**,
/// so reading it off the fixture is what keeps the two arms binding the same
/// ids. A fixture with no `main_table`, or one with no rows in it, cannot
/// measure `prepare.point` and says so here rather than reporting a timing for
/// a query that matched nothing.
///
/// @param imported - the imported copy of the fixture
fn main_table_rows(imported: &Path) -> Result<u32, String> {
    let database =
        Database::open(imported).map_err(|error| format!("open: {}", error.message()))?;
    let connection = database.session();
    let answer = connection
        .query("SELECT count(*) FROM main_table")
        .map_err(|error| {
            format!(
                "the fixture has no main_table to measure prepare.point against: {}",
                error.message()
            )
        })?;
    let counted = match answer.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Int(number)) if *number > 0 => *number,
        _ => {
            return Err(format!(
                "main_table in {} holds no rows, so there is nothing for prepare.point to read",
                imported.display()
            ))
        }
    };
    u32::try_from(counted).map_err(|_| {
        format!("main_table holds {counted} rows, which is more than the bind generator counts in")
    })
}

/// Runs both inillucent arms and the SQLite arm, interleaved, and reports.
///
/// @param options - how many rounds and repeats
/// @param preflight - the files and the row count the run reads
fn run(options: &Options, preflight: &Preflight) -> Result<(), String> {
    // Three arms, one pair of vectors per workload: with the cache, without it,
    // and SQLite.
    let mut cached: Vec<Vec<f64>> = vec![Vec::new(); WORKLOADS.len()];
    let mut uncached: Vec<Vec<f64>> = vec![Vec::new(); WORKLOADS.len()];
    let mut reference: Vec<Vec<f64>> = vec![Vec::new(); WORKLOADS.len()];
    let mut digests: Vec<Option<u64>> = vec![None; WORKLOADS.len()];

    for round in 0..options.rounds {
        // The arm order rotates so no arm is systematically first.
        let order = round % 3;
        for step in 0..3u32 {
            match (order + step) % 3 {
                0 => {
                    let samples = time_inillucent(preflight, options.repeat, 0)?;
                    record(&samples, &mut cached, &mut digests)?;
                }
                1 => {
                    let samples = time_inillucent(preflight, options.repeat, Levers::PLAN_CACHE)?;
                    record(&samples, &mut uncached, &mut digests)?;
                }
                _ => {
                    // The reference reads the fixture SQLite wrote, not the
                    // import - it is a SQLite program and the import is not a
                    // SQLite file.
                    let samples = run_sqlite(&preflight.bench, &preflight.plan, &options.fixture)?;
                    record(&samples, &mut reference, &mut digests)?;
                }
            }
        }
    }

    // Where one prepare-and-step goes, before any of it is compared. Four
    // nested stages over the same statement text, each the one before it plus
    // one step, so the differences are the stages. Without this the plan cache
    // looks like it did not work; with it, the answer is that it worked and
    // that compilation was not the cost.
    breakdown(&preflight.imported, options.repeat)?;
    report(options, &cached, &uncached, &reference);
    Ok(())
}

/// Prints the table, the family ratio and the verdict.
///
/// @param options - how many rounds were taken
/// @param cached - the arm with the plan cache on
/// @param uncached - the arm with the plan cache off
/// @param reference - the SQLite arm
fn report(options: &Options, cached: &[Vec<f64>], uncached: &[Vec<f64>], reference: &[Vec<f64>]) {
    println!(
        "open.prepare, {} rounds of {} prepares, medians in nanoseconds per round",
        options.rounds, options.repeat
    );
    println!(
        "  {:<18} {:>12} {:>12} {:>12} {:>9} {:>9} {:>9}",
        "workload", "cached", "uncached", "sqlite", "cached", "uncached", "cache"
    );
    let mut with_arms: Vec<Paired> = Vec::new();
    let mut without_arms: Vec<Paired> = Vec::new();
    for (index, (name, _, _)) in WORKLOADS.iter().enumerate() {
        let (Some(a), Some(b), Some(c)) =
            (cached.get(index), uncached.get(index), reference.get(index))
        else {
            continue;
        };
        let (with, without, theirs) = (median(a), median(b), median(c));
        with_arms.push(paired_arm(name, a, c));
        without_arms.push(paired_arm(name, b, c));
        println!(
            "  {name:<18} {with:>12.0} {without:>12.0} {theirs:>12.0} {:>8.2}x {:>8.2}x {:>8.2}x",
            theirs / with,
            theirs / without,
            without / with
        );
    }
    // One value per round, the mean of that round's log ratios over the
    // workloads, with the rounds resampled - `perf::family_interval`, the
    // statistic every gate grades a family by. Until task-2093 this pooled
    // every workload's every round into one list, which made the interval
    // measure how far apart `prepare.trivial` and `prepare.point` are.
    let family = |arms: &[Paired]| {
        let members: Vec<&Paired> = arms.iter().collect();
        family_interval(&members, SEED)
    };
    let (with, with_low, with_high) = family(&with_arms);
    let (without, without_low, without_high) = family(&without_arms);
    println!();
    println!("  open.prepare with the cache:    {with:.2}x  ({with_low:.2}x .. {with_high:.2}x)");
    println!(
        "  open.prepare without the cache: {without:.2}x  ({without_low:.2}x .. {without_high:.2}x)"
    );
    println!("  Phase 1 acceptance: at or above 1.00x with the cache");
    println!(
        "  VERDICT: {}",
        if with_low >= 1.0 { "MET" } else { "MISSED" }
    );
}

/// Returns one workload's rounds as pairs of this engine's time and SQLite's.
///
/// @param name - the workload
/// @param ours - this engine's time per round, in nanoseconds
/// @param theirs - SQLite's time per round, in nanoseconds
fn paired_arm(name: &str, ours: &[f64], theirs: &[f64]) -> Paired {
    Paired {
        workload: name.to_string(),
        family: "open.prepare".to_string(),
        pairs: ours.iter().copied().zip(theirs.iter().copied()).collect(),
        agreed: true,
        disagreement: String::new(),
    }
}

/// Prints where one prepare-and-step goes, stage by stage.
///
/// @param imported - the imported fixture to open
/// @param iterations - how many times each stage runs
fn breakdown(imported: &Path, iterations: u32) -> Result<(), String> {
    let database =
        Database::open(imported).map_err(|error| format!("open: {}", error.message()))?;
    let connection = database.session();
    connection
        .execute_batch("PRAGMA busy_timeout = 5000")
        .map_err(|error| format!("busy_timeout: {}", error.message()))?;
    println!("## where one prepare-and-step goes, nanoseconds");
    println!(
        "  {:<18} {:>10} {:>10} {:>10} {:>10} {:>12}",
        "workload", "compile", "prepare", "+bind", "+step", "+step in txn"
    );
    for (name, sql, binds) in WORKLOADS {
        // A cache miss every time: the lever is off, so this is parse, bind,
        // plan and compile plus everything else a prepare does.
        let _ = connection.disable_optimizations(Levers::without(Levers::PLAN_CACHE));
        let compile = stage(iterations, || prepare_only(&connection, name, sql))?;
        // A cache hit every time: everything a prepare does except compiling.
        let _ = connection.disable_optimizations(Levers::all());
        let prepare = stage(iterations, || prepare_only(&connection, name, sql))?;
        let bind = stage(iterations, || {
            prepare_and_bind(&connection, name, sql, binds).map(|_| ())
        })?;
        let step = stage(iterations, || {
            prepare_bind_and_step(&connection, name, sql, binds)
        })?;
        // The same step with a transaction already open. A statement inside
        // one does not take or release the file lock itself, so the difference
        // between this column and the one before it is what the per-statement
        // read transaction costs - which is the hypothesis this column exists
        // to test rather than assert.
        connection
            .execute_batch("BEGIN")
            .map_err(|error| format!("BEGIN: {}", error.message()))?;
        let in_txn = stage(iterations, || {
            prepare_bind_and_step(&connection, name, sql, binds)
        })?;
        connection
            .execute_batch("COMMIT")
            .map_err(|error| format!("COMMIT: {}", error.message()))?;
        println!(
            "  {name:<18} {compile:>10.0} {prepare:>10.0} {bind:>10.0} {step:>10.0} {in_txn:>12.0}"
        );
    }
    let _ = connection.disable_optimizations(Levers::all());
    println!(
        "  the gap between +step and +step in txn is what one statement's implicit read\n  \
         transaction costs. fullgate runs the same workloads through the engine's own\n  \
         pipeline rather than through a connection, so its open.prepare figures and\n  \
         these are not the same measurement."
    );
    println!();
    Ok(())
}

/// Prepares one statement and throws it away.
///
/// @param connection - the open session
/// @param name - the workload, for the message
/// @param sql - the statement text
fn prepare_only(connection: &Connection<'_>, name: &str, sql: &str) -> Result<(), String> {
    let statement = connection
        .prepare(sql)
        .map_err(|error| format!("{name}: {}", error.message()))?;
    drop(statement);
    Ok(())
}

/// Prepares one statement and binds row one to it.
///
/// Row one rather than a scattered id, because this stage is measuring what a
/// bind costs rather than what a lookup costs.
///
/// @param connection - the open session
/// @param name - the workload, for the message
/// @param sql - the statement text
/// @param binds - whether the statement takes a parameter
fn prepare_and_bind<'d>(
    connection: &Connection<'d>,
    name: &str,
    sql: &str,
    binds: bool,
) -> Result<Statement<'d>, String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| format!("{name}: {}", error.message()))?;
    if binds {
        statement
            .bind_integer(1, 1)
            .map_err(|error| format!("{name}: {}", error.message()))?;
    }
    Ok(statement)
}

/// Prepares one statement, binds it and steps it to the end.
///
/// @param connection - the open session
/// @param name - the workload, for the message
/// @param sql - the statement text
/// @param binds - whether the statement takes a parameter
fn prepare_bind_and_step(
    connection: &Connection<'_>,
    name: &str,
    sql: &str,
    binds: bool,
) -> Result<(), String> {
    let mut statement = prepare_and_bind(connection, name, sql, binds)?;
    while statement
        .step()
        .map_err(|error| format!("{name}: {}", error.message()))?
    {}
    Ok(())
}

/// Times one stage and returns nanoseconds per iteration.
///
/// @param iterations - how many times to run the body
/// @param body - the stage to time
fn stage(iterations: u32, mut body: impl FnMut() -> Result<(), String>) -> Result<f64, String> {
    body()?;
    let started = Instant::now();
    for _ in 0..iterations {
        body()?;
    }
    Ok(started.elapsed().as_secs_f64() * 1e9 / f64::from(iterations.max(1)))
}

/// Files one arm's samples into its vectors, checking the answers agree.
///
/// @param samples - what the arm produced
/// @param into - the arm's per-workload timings
/// @param digests - the answer each workload has produced so far
fn record(
    samples: &[Sample],
    into: &mut [Vec<f64>],
    digests: &mut [Option<u64>],
) -> Result<(), String> {
    for (index, (name, _, _)) in WORKLOADS.iter().enumerate() {
        let Some(sample) = samples.iter().find(|sample| sample.workload == *name) else {
            return Err(format!("{name}: no sample"));
        };
        match digests.get_mut(index) {
            Some(Some(held)) if *held != sample.digest => {
                return Err(format!(
                    "{name}: digest {:016x} against {:016x} - the arms disagree",
                    sample.digest, held
                ))
            }
            Some(slot) => *slot = Some(sample.digest),
            None => {}
        }
        if let Some(held) = into.get_mut(index) {
            held.push(sample.nanos);
        }
    }
    Ok(())
}

/// Times the inillucent arm at one lever mask.
///
/// @param preflight - the import to open and the row count to bind against
/// @param repeat - how many prepares one round does
/// @param disabled - the levers to switch off
fn time_inillucent(
    preflight: &Preflight,
    repeat: u32,
    disabled: u32,
) -> Result<Vec<Sample>, String> {
    let database = Database::open(&preflight.imported)
        .map_err(|error| format!("open: {}", error.message()))?;
    let connection = database.session();
    connection
        .execute_batch("PRAGMA busy_timeout = 5000")
        .map_err(|error| format!("busy_timeout: {}", error.message()))?;
    let _ = connection.disable_optimizations(Levers::without(disabled));
    let mut samples = Vec::with_capacity(WORKLOADS.len());
    for (name, sql, binds) in WORKLOADS {
        // One prepare outside the timer so the first-time costs a cache cannot
        // remove - reading the schema, warming the page cache - are not counted
        // against either arm.
        prepare_bind_and_step(&connection, name, sql, binds)?;
        let mut digest = Digest::new();
        let started = Instant::now();
        for iteration in 0..repeat {
            let mut statement = connection
                .prepare(sql)
                .map_err(|error| format!("{name}: {}", error.message()))?;
            if binds {
                // The generator `sqlite_bench.c` uses, called rather than
                // copied: the two arms have to bind the same ids in the same
                // order or the digests disagree and the run stops.
                statement
                    .bind(1, bind_value(Bind::Scatter, iteration, preflight.rows))
                    .map_err(|error| format!("{name}: {}", error.message()))?;
            }
            while statement
                .step()
                .map_err(|error| format!("{name}: {}", error.message()))?
            {
                for value in statement.row() {
                    eat(&mut digest, value);
                }
            }
        }
        samples.push(Sample {
            workload: name.to_string(),
            nanos: started.elapsed().as_secs_f64() * 1e9,
            rows: 0,
            digest: digest.finish(),
        });
    }
    Ok(samples)
}

/// Adds one produced value to the digest, tagged the way the reference tags it.
///
/// @param digest - the running digest
/// @param value - the value to fold in
fn eat(digest: &mut Digest, value: &OwnedDatum) {
    match value {
        OwnedDatum::Null => digest.tag(0),
        OwnedDatum::Int(number) => {
            digest.tag(1);
            digest.word(*number as u64);
        }
        OwnedDatum::Real(number) => {
            digest.tag(2);
            digest.word(number.to_bits());
        }
        OwnedDatum::Text(bytes) => {
            digest.tag(3);
            digest.word(bytes.len() as u64);
            digest.bytes(bytes);
        }
        OwnedDatum::Blob(bytes) => {
            digest.tag(4);
            digest.word(bytes.len() as u64);
            digest.bytes(bytes);
        }
    }
}

/// Renders the plan file `sqlite-bench` reads.
///
/// There is no `scale` line: `compat/oracle/sqlite_bench.c` never reads one,
/// and a name for a size sitting next to the size itself is a second copy that
/// can disagree with `rows`.
///
/// @param rows - how many rows `main_table` holds, which the binds key on
/// @param repeat - how many prepares one round does
fn plan_file(rows: u32, repeat: u32) -> String {
    let mut out = String::new();
    out.push_str("# open.prepare, Phase 1. Both engines read this file.\n");
    out.push_str("version\t1\n");
    out.push_str(&format!("rows\t{rows}\n"));
    out.push_str("journal\tdelete\n");
    out.push_str("synchronous\tfull\n");
    out.push_str("page_size\t4096\n");
    out.push_str("cache_size\t-2000\n");
    for (name, sql, binds) in WORKLOADS {
        out.push_str(&format!("workload\t{name}\n"));
        out.push_str("family\topen.prepare\n");
        out.push_str(&format!("repeat\t{repeat}\n"));
        out.push_str("txn\tnone\n");
        // The whole point of the family: the reference re-prepares too.
        out.push_str("prepare\teach\n");
        if binds {
            out.push_str(&format!("bind\t{}\n", Bind::Scatter.name()));
        }
        out.push_str(&format!("sql\t{sql}\n"));
    }
    out
}

/// Returns the `sqlite-bench` executable, if it has been built.
fn sqlite_bench() -> Option<PathBuf> {
    let root = workspace_root().join(".sqlite-ref/3.53.4");
    for name in ["sqlite-bench.exe", "sqlite-bench"] {
        let candidate = root.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}
