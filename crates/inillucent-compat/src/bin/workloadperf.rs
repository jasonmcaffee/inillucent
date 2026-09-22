//! What a real application's statements cost, on the corpus it was extracted to.
//!
//! Invariant: **every statement timed here is one a consumer actually issues**,
//! read from `tests/workloads/nikaya/statements.sql` rather than written for a
//! benchmark. Nothing about the values: they are generated, and a statement
//! whose value a `CHECK` refuses is still a statement whose compile and plan
//! were paid for.
//!
//! ## Why this exists
//!
//! The corpus has been replayed for correctness by `story_workload_replay`
//! since it was extracted, and never timed (task-2066 §4.3.11). That is a
//! benchmark that costs nothing to add: the statements, their parameters and
//! the schema they run against are already in the tree and already maintained,
//! and the shapes in them are ones a generator does not produce - a compound
//! `SELECT` used as a derived table, a vector search beside an ordinary
//! predicate, a `RETURNING` on an upsert.
//!
//! What the scorecard measures is ten families of workload chosen to be
//! comparable with SQLite's own benchmark. What this measures is one
//! application. They answer different questions and the second one has no
//! reference arm, which is why this reports a distribution and no ratio.
//!
//! ## What it reports
//!
//! The median, the 99th percentile and the slowest statement, over the whole
//! corpus, and then the ten slowest statements by name. The names are the
//! consumer's own source locations, so a slow one is a line somebody can open.
//!
//! **Prepare and run are separated**, because they are different costs with
//! different fixes: a slow prepare is the planner and a slow run is the
//! pipeline, and a corpus of 211 statements issued once each is dominated by
//! the first in a way an application holding its statements is not.
//!
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-workloadperf`

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use inillucent_compat::nikaya::{seed_the_workload, workload, Statement};
use inillucent_compat::platform_name;
use inillucent_compat::stories::Params;
use inillucent_compat::workspace_root;
use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

/// How many times the corpus is replayed.
///
/// Three, and the report is the median across them, for the reason
/// `inillucent-checkpointperf` gives: one run of a corpus this small is one
/// sample of a machine as much as of an engine.
const REPEATS: usize = 3;

/// What one statement cost, in nanoseconds.
struct Timing {
    /// The consumer's own source location.
    source: String,
    /// What `Connection::prepare` took.
    prepare: f64,
    /// What running it took, whatever it answered.
    run: f64,
}

/// Measures the corpus and writes the report.
fn main() -> ExitCode {
    let out = workspace_root().join("_agent_output/measurements/workload");
    if let Err(error) = std::fs::create_dir_all(&out) {
        eprintln!("cannot create {out:?}: {error}");
        return ExitCode::FAILURE;
    }
    let mut runs: Vec<Vec<Timing>> = Vec::with_capacity(REPEATS);
    for repeat in 0..REPEATS {
        match replay(&out, repeat) {
            Ok(timings) => runs.push(timings),
            Err(reason) => {
                eprintln!("{reason}");
                return ExitCode::FAILURE;
            }
        }
    }
    let report = render(&runs);
    println!("{report}");
    if let Err(error) = std::fs::write(out.join("workload.md"), &report) {
        eprintln!("cannot write the report: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Builds a fresh database, replays the corpus once and times each statement.
///
/// A fresh file per repeat, because a statement that inserted a row the first
/// time is refused by a constraint the second, and a run where half the
/// corpus is refused early is not a measurement of the same thing.
///
/// @param out - where the scratch database goes
/// @param repeat - which replay this is, so the files do not collide
fn replay(out: &std::path::Path, repeat: usize) -> Result<Vec<Timing>, String> {
    let path: PathBuf = out.join(format!("workload-{repeat}.rdb"));
    let _ = std::fs::remove_file(&path);
    let (schema, statements) = workload();
    let database = Database::open(&path).map_err(|error| error.message().to_string())?;
    let connection = database.session();
    // The corpus declares `VECTOR(768)`, which is the consumer's embedding
    // width and a fixture rather than a seed; four is enough for the column to
    // be the type it is. `story_workload_replay` narrows it the same way.
    connection
        .execute_batch(&schema.replace("VECTOR(768)", "VECTOR(4)"))
        .map_err(|error| error.message().to_string())?;
    seed_the_workload(&connection);

    let mut timings = Vec::with_capacity(statements.len());
    for statement in &statements {
        timings.push(time_one(&connection, statement));
    }
    if timings.is_empty() {
        return Err("the corpus produced no statement to time".to_string());
    }
    Ok(timings)
}

/// Times one statement's prepare and its run.
///
/// A refusal is timed like anything else: the work of deciding that a value
/// breaks a `CHECK` is work the engine did, and the values in the corpus are
/// generated rather than the consumer's own.
///
/// @param connection - the open connection
/// @param statement - the statement and its parameters
fn time_one(connection: &Connection<'_>, statement: &Statement) -> Timing {
    let prepared = Instant::now();
    let compiled = connection.prepare(&statement.sql);
    let prepare = prepared.elapsed().as_secs_f64() * 1e9;
    let run = match compiled {
        Ok(_) => {
            let params = bind(&statement.params);
            let started = Instant::now();
            let _ = connection.query_with(&statement.sql, &params);
            started.elapsed().as_secs_f64() * 1e9
        }
        // A statement that does not compile has no run to time, and it is a
        // failure of `story_workload_replay` rather than of this.
        Err(_) => 0.0,
    };
    Timing {
        source: statement.source.clone(),
        prepare,
        run,
    }
}

/// Binds one statement's values, one-based the way `?1` is.
///
/// @param values - the parameters the corpus recorded
fn bind(values: &[OwnedDatum]) -> Params {
    let mut params = Params::new();
    for (index, value) in values.iter().enumerate() {
        params.set(
            u32::try_from(index.saturating_add(1)).unwrap_or(1),
            value.clone(),
        );
    }
    params
}

/// Returns the value at a percentile of a sorted-in-place copy.
///
/// @param values - the samples
/// @param percentile - 0.0 to 1.0
fn at(values: &[f64], percentile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut held = values.to_vec();
    held.sort_by(|left, right| {
        left.partial_cmp(right)
            .unwrap_or(core::cmp::Ordering::Equal)
    });
    let last = held.len().saturating_sub(1);
    let index = ((held.len() as f64 - 1.0) * percentile).round() as usize;
    held.get(index.min(last)).copied().unwrap_or(0.0)
}

/// Returns the median across runs of one statistic of each run.
///
/// @param runs - each replay's timings
/// @param of - the statistic to take from one replay
fn across(runs: &[Vec<Timing>], of: impl Fn(&[Timing]) -> f64) -> f64 {
    let mut values: Vec<f64> = runs.iter().map(|run| of(run)).collect();
    values.sort_by(|left, right| {
        left.partial_cmp(right)
            .unwrap_or(core::cmp::Ordering::Equal)
    });
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

/// Renders the report.
///
/// @param runs - each replay's timings
fn render(runs: &[Vec<Timing>]) -> String {
    let statements = runs.first().map(Vec::len).unwrap_or(0);
    let mut out = String::new();
    out.push_str("# What one application's statements cost\n\n");
    out.push_str(&format!(
        "Platform `{platform}`, {statements} statements from \
         `tests/workloads/nikaya/statements.sql`, {REPEATS} replays, each number the median \
         across replays of that replay's own statistic, in nanoseconds. There is no reference \
         arm: this is a distribution over one consumer's own SQL, not a comparison.\n\n",
        platform = platform_name()
    ));
    out.push_str("| statistic | prepare | run |\n|---|---:|---:|\n");
    for (name, percentile) in [
        ("median", 0.50),
        ("90th percentile", 0.90),
        ("99th percentile", 0.99),
    ] {
        out.push_str(&format!(
            "| {name} | {:.0} | {:.0} |\n",
            across(runs, |run| at(
                &run.iter().map(|held| held.prepare).collect::<Vec<f64>>(),
                percentile
            )),
            across(runs, |run| at(
                &run.iter().map(|held| held.run).collect::<Vec<f64>>(),
                percentile
            )),
        ));
    }
    out.push_str(&format!(
        "| **slowest** | **{:.0}** | **{:.0}** |\n",
        across(runs, |run| at(
            &run.iter().map(|held| held.prepare).collect::<Vec<f64>>(),
            1.0
        )),
        across(runs, |run| at(
            &run.iter().map(|held| held.run).collect::<Vec<f64>>(),
            1.0
        )),
    ));
    out.push_str(&format!(
        "| whole corpus | {:.0} | {:.0} |\n\n",
        across(runs, |run| run.iter().map(|held| held.prepare).sum()),
        across(runs, |run| run.iter().map(|held| held.run).sum()),
    ));
    out.push_str(&slowest_table(runs));
    out.push_str(
        "\nA slow prepare is the planner and a slow run is the pipeline, which is why they are \
         two columns. A corpus issued once each is dominated by the prepare in a way an \
         application that holds its statements is not.\n",
    );
    out
}

/// Renders the ten slowest statements by total cost, named.
///
/// @param runs - each replay's timings
fn slowest_table(runs: &[Vec<Timing>]) -> String {
    let Some(first) = runs.first() else {
        return String::new();
    };
    let mut totals: Vec<(String, f64, f64)> = Vec::with_capacity(first.len());
    for (index, held) in first.iter().enumerate() {
        let prepare = across(runs, |run| {
            run.get(index).map(|one| one.prepare).unwrap_or(0.0)
        });
        let run_ns = across(runs, |run| run.get(index).map(|one| one.run).unwrap_or(0.0));
        totals.push((held.source.clone(), prepare, run_ns));
    }
    totals.sort_by(|left, right| {
        (right.1 + right.2)
            .partial_cmp(&(left.1 + left.2))
            .unwrap_or(core::cmp::Ordering::Equal)
    });
    let mut out = String::from("The ten slowest, by prepare plus run:\n\n");
    out.push_str("| statement | prepare | run |\n|---|---:|---:|\n");
    for (source, prepare, run_ns) in totals.iter().take(10) {
        out.push_str(&format!("| `{source}` | {prepare:.0} | {run_ns:.0} |\n"));
    }
    out
}
