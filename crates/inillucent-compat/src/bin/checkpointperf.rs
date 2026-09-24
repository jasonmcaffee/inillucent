//! What an explicit checkpoint costs the commit that runs beside it.
//!
//! Invariant: this measures a *distribution*, not a total, because the
//! question is not whether checkpointing costs anything in aggregate but
//! whether it costs one commit disproportionately. The numbers here are the
//! median, the 99th percentile and the worst single commit.
//!
//! The worst commit is the one an application notices. A write path whose
//! median is a hundred microseconds and whose worst is forty milliseconds is a
//! write path with a stall in it, and the stall is not visible in any average.
//!
//! **This file used to measure a different lever: the old engine ran an
//! *automatic* checkpoint once the log crossed a size threshold, and a
//! `set_checkpoint_budget` lever decided whether that checkpoint copied the
//! whole log into the database file in one commit or a hundred frames at a
//! time.** The new engine has no such lever because it has no automatic
//! checkpoint at all - `inillucent_engine::pragma`'s own `reported_value` for
//! `wal_autocheckpoint` says so directly: "the log is folded in at an explicit
//! checkpoint rather than every N frames, so there is no frame count to set."
//! So the two arms this file now measures are the ones the new engine actually
//! has: committing with an explicit `Database::checkpoint()` folded in every
//! `BUDGET` commits ("spread"), against growing the log for the whole run and
//! checkpointing once at the end ("all at once"). The question survives even
//! though the lever changed - does folding the log in periodically cost one of
//! its commits a stall the deferred approach does not.
//!
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-checkpointperf`

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use inillucent_compat::{platform_name, workspace_root};
use inillucent_engine::connect::Database;

/// How many rows one run commits, one transaction each.
const COMMITS: usize = 6_000;

/// How many commits the spread arm runs between explicit checkpoints.
const BUDGET: usize = 100;

/// How many times each arm is run.
const REPEATS: usize = 3;

/// Measures both arms and writes the report.
fn main() -> ExitCode {
    // Ticket-neutral, and beside the scorecard the release gathers from, for
    // the same reason the scorecard's own default is: a measurement of this
    // build does not belong in an earlier ticket's folder.
    let out = workspace_root().join("_agent_output/measurements/checkpoint");
    if let Err(error) = std::fs::create_dir_all(&out) {
        eprintln!("cannot create {out:?}: {error}");
        return ExitCode::FAILURE;
    }
    let mut scheduled: Vec<Sample> = Vec::new();
    let mut all_at_once: Vec<Sample> = Vec::new();
    for repeat in 0..REPEATS {
        // Alternate the order so neither arm is always the cold one.
        let order = if repeat % 2 == 0 {
            [true, false]
        } else {
            [false, true]
        };
        for spread in order {
            match run(&out, spread, repeat) {
                Ok(sample) if spread => scheduled.push(sample),
                Ok(sample) => all_at_once.push(sample),
                Err(reason) => {
                    eprintln!("{reason}");
                    return ExitCode::FAILURE;
                }
            }
        }
    }
    let report = render(&scheduled, &all_at_once);
    println!("{report}");
    // **Beside the table, for the release gate to read** (task-2066 §4.3.11).
    // The markdown is for a person; `inillucent-release` needs two numbers per
    // arm and should not be parsing a table to get them.
    let numbers = format!(
        "arm\tmedian_us\tworst_us\n\
         scheduled\t{:.1}\t{:.1}\n\
         all_at_once\t{:.1}\t{:.1}\n",
        across(&scheduled, |sample| sample.at(0.50)),
        across(&scheduled, |sample| sample.at(1.0)),
        across(&all_at_once, |sample| sample.at(0.50)),
        across(&all_at_once, |sample| sample.at(1.0)),
    );
    if let Err(error) = std::fs::write(out.join("checkpoint.tsv"), &numbers) {
        eprintln!("cannot write the checkpoint numbers: {error}");
        return ExitCode::FAILURE;
    }
    if let Err(error) = std::fs::write(out.join("checkpoint.md"), &report) {
        eprintln!("cannot write the report: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// What one run of one arm measured.
struct Sample {
    /// Every commit's duration, in microseconds, sorted.
    commits: Vec<f64>,
    /// How many checkpoints ran.
    checkpoints: u64,
    /// How many log records were appended across the whole run.
    records: u64,
    /// How many bytes the log grew by across the whole run.
    bytes: u64,
}

impl Sample {
    /// Returns the value at a percentile of the sorted commits.
    fn at(&self, percentile: f64) -> f64 {
        if self.commits.is_empty() {
            return 0.0;
        }
        let last = self.commits.len().saturating_sub(1);
        let at = ((self.commits.len() as f64 - 1.0) * percentile).round() as usize;
        self.commits.get(at.min(last)).copied().unwrap_or(0.0)
    }

    /// Returns the total time the run took.
    fn total(&self) -> f64 {
        self.commits.iter().sum()
    }
}

/// Runs one arm against a fresh database and times every commit.
///
/// `synchronous` is `normal`, which is what a write-ahead log is usually run
/// at: at `full` every commit pays an fsync that is larger than the effect
/// being measured, and the checkpoint would be a ripple on it.
/// @param out - where the databases are built
/// @param spread - whether an explicit checkpoint runs every `BUDGET` commits
/// @param repeat - which repeat this is, so the file names do not collide
fn run(out: &std::path::Path, spread: bool, repeat: usize) -> Result<Sample, String> {
    let path: PathBuf = out.join(format!(
        "checkpoint-{}-{repeat}.db",
        if spread { "spread" } else { "at-once" }
    ));
    inillucent_base::testing::remove_database(&path);
    let database = Database::open(&path).map_err(|error| error.message().to_string())?;
    let connection = database.session();
    for pragma in [
        "PRAGMA journal_mode=wal;",
        "PRAGMA synchronous=normal;",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, label TEXT, payload BLOB);",
    ] {
        connection
            .execute_batch(pragma)
            .map_err(|error| format!("{pragma}: {}", error.message()))?;
    }

    let mut commits = Vec::with_capacity(COMMITS);
    let mut checkpoints = 0u64;
    for row in 0..COMMITS {
        let sql = format!(
            "INSERT INTO t(id, label, payload) VALUES ({row}, 'row {row} of the checkpoint \
             benchmark', zeroblob(256))"
        );
        let at = Instant::now();
        connection
            .execute_batch(&sql)
            .map_err(|error| format!("insert {row}: {}", error.message()))?;
        if spread && row > 0 && row % BUDGET == 0 {
            database
                .checkpoint()
                .map_err(|error| format!("checkpoint at row {row}: {}", error.message()))?;
            checkpoints = checkpoints.saturating_add(1);
        }
        commits.push(at.elapsed().as_secs_f64() * 1.0e6);
    }
    // The counters, so the report can say whether the thing being measured
    // happened at all. A tail difference between two arms that never wrote to
    // the log differently would be a tail difference about something else.
    let stats = database.log_stats();
    if !spread {
        database
            .checkpoint()
            .map_err(|error| format!("final checkpoint: {}", error.message()))?;
        checkpoints = checkpoints.saturating_add(1);
    }
    commits.sort_by(|left, right| {
        left.partial_cmp(right)
            .unwrap_or(core::cmp::Ordering::Equal)
    });
    Ok(Sample {
        commits,
        checkpoints,
        records: stats.records,
        bytes: stats.bytes,
    })
}

/// Returns the median of a set of runs' statistics.
fn across(samples: &[Sample], of: impl Fn(&Sample) -> f64) -> f64 {
    let mut values: Vec<f64> = samples.iter().map(of).collect();
    values.sort_by(|left, right| {
        left.partial_cmp(right)
            .unwrap_or(core::cmp::Ordering::Equal)
    });
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

/// Renders the report.
fn render(scheduled: &[Sample], all_at_once: &[Sample]) -> String {
    let mut out = String::new();
    out.push_str("# An explicit checkpoint, spread over commits and not\n\n");
    out.push_str(&format!(
        "Platform `{}`, {COMMITS} single-row transactions per run, {REPEATS} runs per arm, \
         write-ahead log at `synchronous=normal`. Each number is the median across runs of that \
         run's own statistic, in microseconds.\n\n",
        platform_name()
    ));
    out.push_str("| statistic | checkpoint every 100 commits | one checkpoint at the end |\n|---|---:|---:|\n");
    for (name, percentile) in [
        ("median commit", 0.50),
        ("90th percentile", 0.90),
        ("99th percentile", 0.99),
        ("99.9th percentile", 0.999),
    ] {
        out.push_str(&format!(
            "| {name} | {:.1} | {:.1} |\n",
            across(scheduled, |sample| sample.at(percentile)),
            across(all_at_once, |sample| sample.at(percentile))
        ));
    }
    out.push_str(&format!(
        "| **worst commit** | **{:.1}** | **{:.1}** |\n",
        across(scheduled, |sample| sample.at(1.0)),
        across(all_at_once, |sample| sample.at(1.0))
    ));
    out.push_str(&format!(
        "| checkpoints run | {:.0} | {:.0} |\n",
        across(scheduled, |sample| sample.checkpoints as f64),
        across(all_at_once, |sample| sample.checkpoints as f64)
    ));
    out.push_str(&format!(
        "| log records | {:.0} | {:.0} |\n",
        across(scheduled, |sample| sample.records as f64),
        across(all_at_once, |sample| sample.records as f64)
    ));
    out.push_str(&format!(
        "| log bytes | {:.0} | {:.0} |\n",
        across(scheduled, |sample| sample.bytes as f64),
        across(all_at_once, |sample| sample.bytes as f64)
    ));
    out.push_str(&format!(
        "| total, all commits | {:.0} | {:.0} |\n\n",
        across(scheduled, Sample::total),
        across(all_at_once, Sample::total)
    ));
    out.push_str(
        "The log records and bytes are the controls: both arms write the same rows to the same \
         file, so a difference there would mean an arm had changed the work rather than its \
         distribution.\n\n",
    );
    out.push_str(
        "The finding this file reports is whichever arm's worst commit and tail percentiles are \
         higher: an explicit checkpoint folds every dirty page still in the log into the file, so \
         a checkpoint run mid-batch pays for everything since the last one, and running it more \
         often trades a larger number of smaller pauses for a smaller number of larger ones.\n",
    );
    out
}
