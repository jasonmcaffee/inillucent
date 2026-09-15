//! `open.prepare` on the shipped engine, with the plan cache and without it.
//!
//! Invariant: both arms prepare the *same* SQL against the *same* database and
//! step the resulting statement, and each arm's answer is checked before its
//! time is read. The only difference between them is one lever.
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
//! Usage: inillucent-prepareperf `<sqlite fixture>` `rounds`

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use inillucent_compat::perf::run_sqlite;
use inillucent_compat::perf::{bootstrap, median, Digest, Sample};
use inillucent_compat::workspace_root;
use inillucent_engine::connect::Database;
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

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(fixture) = arguments.first() else {
        eprintln!("usage: inillucent-prepareperf <sqlite fixture> [rounds]");
        return ExitCode::from(2);
    };
    let rounds: u32 = arguments
        .get(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(30);
    match run(&PathBuf::from(fixture), rounds) {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Runs both inillucent arms and the SQLite arm, interleaved.
///
/// @param fixture - the database all three read
/// @param rounds - how many paired rounds
fn run(fixture: &Path, rounds: u32) -> Result<(), String> {
    let bench = sqlite_bench().ok_or_else(|| "sqlite-bench is not built".to_string())?;
    let plan_path = std::env::temp_dir().join(format!(
        "inillucent-prepareperf-{}.plan",
        std::process::id()
    ));
    std::fs::write(&plan_path, plan_file()).map_err(|error| format!("plan: {error}"))?;

    // Three arms, one pair of vectors per workload: with the cache, without it,
    // and SQLite.
    let mut cached: Vec<Vec<f64>> = vec![Vec::new(); WORKLOADS.len()];
    let mut uncached: Vec<Vec<f64>> = vec![Vec::new(); WORKLOADS.len()];
    let mut reference: Vec<Vec<f64>> = vec![Vec::new(); WORKLOADS.len()];
    let mut digests: Vec<Option<u64>> = vec![None; WORKLOADS.len()];

    for round in 0..rounds {
        // The arm order rotates so no arm is systematically first.
        let order = round % 3;
        for step in 0..3u32 {
            match (order + step) % 3 {
                0 => {
                    let samples = time_inillucent(fixture, 0)?;
                    record(&samples, &mut cached, &mut digests)?;
                }
                1 => {
                    let samples = time_inillucent(fixture, Levers::PLAN_CACHE)?;
                    record(&samples, &mut uncached, &mut digests)?;
                }
                _ => {
                    let samples = run_sqlite(&bench, &plan_path, fixture)?;
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
    breakdown(fixture)?;

    println!("open.prepare, {rounds} rounds, medians in nanoseconds per round");
    println!(
        "  {:<18} {:>12} {:>12} {:>12} {:>9} {:>9} {:>9}",
        "workload", "cached", "uncached", "sqlite", "cached", "uncached", "cache"
    );
    let mut with_logs: Vec<f64> = Vec::new();
    let mut without_logs: Vec<f64> = Vec::new();
    for (index, (name, _, _)) in WORKLOADS.iter().enumerate() {
        let (Some(a), Some(b), Some(c)) =
            (cached.get(index), uncached.get(index), reference.get(index))
        else {
            continue;
        };
        let (with, without, theirs) = (median(a), median(b), median(c));
        for (ours, sqlite) in a.iter().zip(c.iter()) {
            with_logs.push((sqlite / ours).ln());
        }
        for (ours, sqlite) in b.iter().zip(c.iter()) {
            without_logs.push((sqlite / ours).ln());
        }
        println!(
            "  {name:<18} {with:>12.0} {without:>12.0} {theirs:>12.0} {:>8.2}x {:>8.2}x {:>8.2}x",
            theirs / with,
            theirs / without,
            without / with
        );
    }
    let family = |logs: &[f64]| {
        let mean = logs.iter().sum::<f64>() / logs.len().max(1) as f64;
        let (low, high) = bootstrap(logs, SEED);
        (mean.exp(), low.exp(), high.exp())
    };
    let (with, with_low, with_high) = family(&with_logs);
    let (without, without_low, without_high) = family(&without_logs);
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
    Ok(())
}

/// Prints where one prepare-and-step goes, stage by stage.
///
/// @param fixture - the database to open
fn breakdown(fixture: &Path) -> Result<(), String> {
    let database = Database::open(fixture).map_err(|error| format!("open: {}", error.message()))?;
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
        let compile = stage(4_000, || {
            let statement = connection
                .prepare(sql)
                .map_err(|error| format!("{name}: {}", error.message()))?;
            drop(statement);
            Ok(())
        })?;
        // A cache hit every time: everything a prepare does except compiling.
        let _ = connection.disable_optimizations(Levers::all());
        let prepare = stage(4_000, || {
            let statement = connection
                .prepare(sql)
                .map_err(|error| format!("{name}: {}", error.message()))?;
            drop(statement);
            Ok(())
        })?;
        let bind = stage(4_000, || {
            let mut statement = connection
                .prepare(sql)
                .map_err(|error| format!("{name}: {}", error.message()))?;
            if binds {
                statement
                    .bind_integer(1, 1)
                    .map_err(|error| format!("{name}: {}", error.message()))?;
            }
            Ok(())
        })?;
        let step = stage(4_000, || {
            let mut statement = connection
                .prepare(sql)
                .map_err(|error| format!("{name}: {}", error.message()))?;
            if binds {
                statement
                    .bind_integer(1, 1)
                    .map_err(|error| format!("{name}: {}", error.message()))?;
            }
            while statement
                .step()
                .map_err(|error| format!("{name}: {}", error.message()))?
            {}
            Ok(())
        })?;
        // The same step with a transaction already open. A statement inside
        // one does not take or release the file lock itself, so the difference
        // between this column and the one before it is what the per-statement
        // read transaction costs - which is the hypothesis this column exists
        // to test rather than assert.
        connection
            .execute_batch("BEGIN")
            .map_err(|error| format!("BEGIN: {}", error.message()))?;
        let in_txn = stage(4_000, || {
            let mut statement = connection
                .prepare(sql)
                .map_err(|error| format!("{name}: {}", error.message()))?;
            if binds {
                statement
                    .bind_integer(1, 1)
                    .map_err(|error| format!("{name}: {}", error.message()))?;
            }
            while statement
                .step()
                .map_err(|error| format!("{name}: {}", error.message()))?
            {}
            Ok(())
        })?;
        connection
            .execute_batch("COMMIT")
            .map_err(|error| format!("COMMIT: {}", error.message()))?;
        println!(
            "  {name:<18} {compile:>10.0} {prepare:>10.0} {bind:>10.0} {step:>10.0} {in_txn:>12.0}"
        );
    }
    let _ = connection.disable_optimizations(Levers::all());
    println!();
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
    Ok(started.elapsed().as_secs_f64() * 1e9 / f64::from(iterations))
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
/// @param fixture - the database to open
/// @param disabled - the levers to switch off
fn time_inillucent(fixture: &Path, disabled: u32) -> Result<Vec<Sample>, String> {
    let database = Database::open(fixture).map_err(|error| format!("open: {}", error.message()))?;
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
        {
            let mut warm = connection
                .prepare(sql)
                .map_err(|error| format!("{name}: {}", error.message()))?;
            if binds {
                warm.bind_integer(1, 1)
                    .map_err(|error| format!("{name}: {}", error.message()))?;
            }
            while warm
                .step()
                .map_err(|error| format!("{name}: {}", error.message()))?
            {}
        }
        let mut digest = Digest::new();
        let started = Instant::now();
        for iteration in 0..REPEAT {
            let mut statement = connection
                .prepare(sql)
                .map_err(|error| format!("{name}: {}", error.message()))?;
            if binds {
                statement
                    .bind_integer(1, scatter(iteration))
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

/// How many times each workload prepares inside one timed round.
///
/// `repeats_for("medium").0` in `scorecard.rs`, which is what the
/// `open.prepare` family uses.
const REPEAT: u32 = 4_000;

/// The parameter the scorecard's `Bind::Scatter` produces.
///
/// A copy of the generator both engines share, so the two arms bind the same
/// values in the same order.
///
/// @param iteration - which iteration of the loop
fn scatter(iteration: u32) -> i64 {
    // A copy of `bind_one`'s `Bind::Scatter` arm in `scorecard.rs`, arithmetic
    // for arithmetic, over the medium scale's 100,000 rows. The stride is a
    // large odd multiplier so successive iterations touch unrelated pages, and
    // getting it wrong would have the two engines reading different rows.
    let scattered = (u64::from(iteration)).wrapping_mul(2_654_435_761) % 100_000u64;
    1 + scattered as i64
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
fn plan_file() -> String {
    let mut out = String::new();
    out.push_str("# open.prepare, Phase 1. Both engines read this file.\n");
    out.push_str("version\t1\n");
    out.push_str("scale\tmedium\n");
    out.push_str("rows\t100000\n");
    out.push_str("journal\tdelete\n");
    out.push_str("synchronous\tfull\n");
    out.push_str("page_size\t4096\n");
    out.push_str("cache_size\t-2000\n");
    for (name, sql, binds) in WORKLOADS {
        out.push_str(&format!("workload\t{name}\n"));
        out.push_str("family\topen.prepare\n");
        out.push_str(&format!("repeat\t{REPEAT}\n"));
        out.push_str("txn\tnone\n");
        // The whole point of the family: the reference re-prepares too.
        out.push_str("prepare\teach\n");
        if binds {
            out.push_str("bind\tscatter\n");
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
