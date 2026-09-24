//! The Phase 3 gate: the write and transaction families, against pinned SQLite.
//!
//! Invariant: **every round starts both engines from the same database.** A
//! write workload changes what it measures, so the second round of an unrestored
//! fixture is measuring a different table than the first - more rows, a deeper
//! tree, a different set of keys already present. Each arm of each round is
//! therefore handed a **fresh copy of the fixture**, made outside the timed
//! region, and the copy is what it writes to.
//!
//! That is the one thing this gate has to get right that the read gate did not,
//! and it is why this is a separate binary rather than a family list added to
//! that one: the read gate's whole structure assumes the database is the same
//! at the end of a round as at the start, and it is right to assume it.
//!
//! ## The four fairness questions, unchanged
//!
//! **Same SQL, same repeat count, same grouping** - the workloads come from
//! `inillucent_compat::perf::plan_for`, the function the scorecard itself calls,
//! and `Grouping` decides the transaction boundaries on both sides.
//!
//! **Same setup, and it took task-2029 to notice it was missing.** A workload's
//! `pre` runs on both arms, outside the clock, where `sqlite_bench.c` runs it.
//! This gate ran it on neither, which meant SQLite reset `side_table.note`
//! twice a round and inillucent never did, every round ended with the two arms
//! holding different text, and the gate refused to time any of them. See
//! [`time_one`] for what the numbers were.
//!
//! **Same cache size** - `--frames` sets the pool and SQLite's `cache_size` is
//! set to the same number of bytes in the plan both arms read. Both numbers are
//! printed and the report says whether they match.
//!
//! **Same durability** - and this one is new, because it does not arise for a
//! read. A write benchmark against an engine that does not sync is not a
//! benchmark, it is a demonstration. Both arms run at `synchronous = FULL`, and
//! the setting is printed.
//!
//! ## What "agreed" means for a write
//!
//! Not the rows the statement returned - a write returns none, and comparing
//! that number to itself proves nothing. It is the **state of the database
//! afterwards**: at the end of every round each arm is asked the same
//! arithmetic questions about every table the workloads touch - how many rows,
//! and the sums of their integer columns - and a round whose answers differ is
//! reported as a correctness failure and is not timed.
//!
//! Sums rather than a value-by-value digest because the two arms render text and
//! blobs differently at the shell boundary, and a check that failed on
//! formatting would be a check nobody could act on. Sums over `count(*)`, the
//! keys and the changed columns move if the write path inserted the wrong rows,
//! updated the wrong ones, or deleted the wrong ones - which is what this gate
//! needs to rule out before it reads a clock. What was written *exactly* is
//! `new_engine_writes.rs`'s job, and it asks a far stronger question: SQLite,
//! about every index, after every statement.
//!
//! Usage:
//!   inillucent-writegate `<sqlite fixture>` [--rounds N] [--page-size N]
//!                        [--scale S] [--frames N] [--families a,b] [--repeat N]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use inillucent_compat::affinity::{self, Placement};
use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::perf::bind_value;
use inillucent_compat::perf::{plan_for, Grouping, Paired, Sample, Workload};
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// The seed the bootstrap resamples with, so an interval is reproducible.
const SEED: u64 = 0x5eed_1832;

/// The families this gate covers, with the TDD's bar for each.
///
/// The bars are the ticket's Phase 3 acceptance, quoted: "`write` at least
/// 1.5x, `transaction` at least 1.0x". Each is a **lower 95% bound**, not a
/// point estimate - a ratio that clears the bar with an interval straddling it
/// has not cleared it.
const FAMILIES: [(&str, f64); 2] = [("write", 1.5), ("transaction", 1.0)];

/// What the gate was asked to measure.
struct Settings {
    rounds: u32,
    page_size: usize,
    frames: usize,
    scale: String,
    families: Vec<String>,
    repeat_override: Option<u32>,
    /// Whether a busy machine stops the pass being graded, and whether to record a reference (task-2110).
    quiet: inillucent_compat::quiet::Options,
}

fn main() -> ExitCode {
    let mut arguments: Vec<String> = std::env::args().skip(1).collect();
    let cores = match affinity::take_cores_flag(&mut arguments) {
        Ok(cores) => cores,
        Err(reason) => {
            eprintln!("write gate: {reason}");
            return ExitCode::from(2);
        }
    };
    let Some(fixture) = arguments.first().filter(|first| !first.starts_with("--")) else {
        eprintln!(
            "usage: inillucent-writegate <sqlite fixture> [--rounds N] [--page-size N] \
             [--scale S] [--frames N] [--families a,b] [--repeat N] \
             [--cores performance|efficiency|any] [--quiet-threshold PERCENT]              [--record-quiet-reference]"
        );
        return ExitCode::from(2);
    };
    // **Pinned before any child exists (task-2085).** The child inherits the
    // mask, and `time_sqlite` checks that it did.
    let placement = match affinity::pin(cores) {
        Ok(placement) => placement,
        Err(reason) => {
            eprintln!(
                "write gate: could not pin to the {} cores: {reason}",
                cores.name()
            );
            return ExitCode::from(2);
        }
    };
    let settings = Settings {
        rounds: flag(&arguments, "--rounds")
            .and_then(|value| value.parse().ok())
            .unwrap_or(30),
        page_size: flag(&arguments, "--page-size")
            .and_then(|value| value.parse().ok())
            .unwrap_or(8_192),
        frames: flag(&arguments, "--frames")
            .and_then(|value| value.parse().ok())
            .unwrap_or(4_096),
        scale: flag(&arguments, "--scale").unwrap_or_else(|| "medium".to_string()),
        families: flag(&arguments, "--families")
            .map(|value| value.split(',').map(str::to_string).collect())
            .unwrap_or_else(|| FAMILIES.iter().map(|(name, _)| name.to_string()).collect()),
        repeat_override: flag(&arguments, "--repeat").and_then(|value| value.parse().ok()),
        quiet: inillucent_compat::quiet::Options::from_arguments(&arguments),
    };
    match run(Path::new(fixture), &settings, &placement) {
        Ok(Some(true)) => ExitCode::SUCCESS,
        Ok(Some(false)) => ExitCode::from(1),
        // Measured and not graded, because the machine was not quiet (task-2110).
        Ok(None) => ExitCode::from(inillucent_compat::quiet::NOT_GRADED),
        Err(reason) => {
            eprintln!("write gate: {reason}");
            ExitCode::from(2)
        }
    }
}

/// Returns a flag's value, when it was given.
///
/// @param arguments - the command line
/// @param name - the flag, with its dashes
fn flag(arguments: &[String], name: &str) -> Option<String> {
    let at = arguments.iter().position(|value| value == name)?;
    arguments.get(at.saturating_add(1)).cloned()
}

/// Returns an error's detail, for a message.
///
/// @param error - the error
fn why(error: &inillucent_base::DbError) -> String {
    error.detail().unwrap_or(error.message()).to_string()
}

/// The questions both arms are asked at the end of a round.
///
/// Arithmetic over every table the write workloads touch. Each answer is a row
/// of integers, which both a shell and an engine render the same way - unlike
/// text and blobs, where the two disagree about quoting and hex and a mismatch
/// would say nothing about the writes.
const AGREEMENT: [&str; 3] = [
    "SELECT count(*), sum(id), sum(key), sum(category) FROM main_table",
    "SELECT count(*), sum(id), sum(owner), sum(length(note)) FROM side_table",
    "SELECT count(*), sum(id), sum(length(body)) FROM wide",
];

/// Runs the gate and reports whether every family's bar was met.
///
/// @param fixture - the SQLite database both engines read
/// @param settings - what to measure and how
/// Prints the configuration block that opens the report.
///
/// **Lifted out of [`run`] because a banner is not a measurement
/// (task-1969, 7.2).** `run` was 318 lines and the ratchet in `policy.rs`
/// froze it there rather than shrinking it, which is how a criterion reading
/// "no production function over 300 lines" was satisfied with eight functions
/// over 300. This is the first seam: everything here is about telling a reader
/// what was run, and none of it decides anything.
///
/// The block is what makes a report comparable between machines - the pool
/// size, SQLite's cache in the same units, the lock mode - so it is printed
/// before anything is measured and never conditionally.
///
/// @param settings - the command line this run was given
/// @param plan - the workload plan both arms run
/// @param pool_bytes - the page pool's size, which the report states in MiB
/// @param placement - the processors both arms run on
fn print_configuration(
    settings: &Settings,
    plan: &inillucent_compat::perf::Plan,
    pool_bytes: usize,
    placement: &Placement,
) {
    println!("## configuration");
    // First, because an unpinned run on a hybrid processor can put the two
    // arms on different core classes and nothing else here would show it
    // (task-2085).
    placement.print_configuration();
    println!("  scale       : {}", settings.scale);
    println!("  rounds      : {}", settings.rounds);
    println!(
        "  pool        : {} frames of {} bytes = {:.1} MiB",
        settings.frames,
        settings.page_size,
        pool_bytes as f64 / (1024.0 * 1024.0)
    );
    println!(
        "  sqlite cache: {} = {:.1} MiB",
        plan.cache_size,
        -(plan.cache_size as f64) / 1024.0
    );
    println!("  fairness    : matched - one memory budget, both engines");
    println!("  durability  : synchronous = FULL on both arms");
    println!();
}

/// Prints the per-family verdict table, and returns whether every family met
/// its bar.
///
/// **Lifted out of [`run`] (task-1969, 7.2).** `run` was 302 lines, and this
/// is one stage of it: it reads what was measured and answers one question.
/// The answer comes back as a `bool` rather than being written into a captured
/// binding, which is what makes it a stage rather than a block.
///
/// A family with no data is a miss, not a pass. That is the whole reason this
/// returns a value: a gate that printed `NO DATA` and went on to say `MET`
/// would be the defect `gates_fail_closed.rs` exists to refuse.
///
/// @param settings - the command line, for which families were asked for
/// @param measured - every workload's paired rounds
/// @param graded - false when the machine was not quiet, so no family is MET or MISSED
fn report_families(settings: &Settings, measured: &[Paired], graded: bool) -> bool {
    let mut met_every_family = true;
    println!();
    println!("## families");
    println!(
        "  {:<14} {:>9} {:>9} {:>9} {:>8} {:>9}  verdict",
        "family", "ratio", "low", "high", "bar", "worst"
    );
    for (family, bar) in FAMILIES {
        if !settings.families.iter().any(|name| name == family) {
            continue;
        }
        let members: Vec<&Paired> = measured
            .iter()
            .filter(|entry| entry.family == family && entry.agreed && !entry.pairs.is_empty())
            .collect();
        if members.is_empty() {
            println!(
                "  {family:<14} {:>9} {:>9} {:>9} {bar:>7.2}x  NO DATA",
                "-", "-", "-"
            );
            met_every_family = false;
            continue;
        }
        // **The family is every workload's every round, weighted per workload.**
        //
        // Two things had to be got right here and the first attempt got both
        // wrong. Pooling the raw `(ours, theirs)` pairs - which is what the read
        // gate does - lets a workload with a hundred times the absolute time
        // decide the family on its own, and it mixes two statistics: the point
        // estimate is a *median* of log ratios while the interval bootstraps
        // their *mean*, so over a heterogeneous pool the estimate can fall
        // outside its own interval. The first medium run printed `write` at
        // ratio 0.38x with a lower bound of 0.40x, which is not a number
        // anybody can act on.
        //
        // Collapsing each workload to its median first fixes the weighting and
        // breaks the interval instead: with three workloads a bootstrap of
        // three points has a 1-in-27 chance of drawing the minimum three times,
        // so its 2.5th percentile *is* the minimum. `transaction`'s lower bound
        // was its worst workload's ratio, exactly, and no amount of data would
        // have moved it.
        //
        // The third attempt put one log ratio per workload per round into one
        // list and bootstrapped it, and task-2086 found that wrong too: a
        // resample draws the workloads in random proportions, so when they
        // differ the interval measures the gap between them rather than the
        // timing noise. `read.join` missed its bar on every build for that
        // reason alone.
        //
        // So: one value per round, the mean of that round's log ratios over
        // the family's workloads, and the bootstrap resamples rounds - which
        // is how the headline already treats each family. Each workload
        // counts equally whatever it costs in nanoseconds. The `worst` column
        // carries the workload holding the family back, and the per-workload
        // table above carries the rest.
        let rolled = Paired {
            workload: family.to_string(),
            family: family.to_string(),
            pairs: members
                .iter()
                .flat_map(|entry| entry.pairs.iter().copied())
                .collect(),
            agreed: true,
            disagreement: String::new(),
        };
        let (low, high) = family_bounds(&members, SEED);
        let worst = members
            .iter()
            .map(|entry| entry.ratio())
            .fold(f64::INFINITY, f64::min);
        // **The lower bound against the bar, not the point estimate.** A ratio
        // that clears a bar with an interval straddling it has not cleared it,
        // and saying otherwise is the one thing a gate must never do.
        let met = low >= bar;
        met_every_family = met_every_family && met;
        println!(
            "  {family:<14} {:>8.2}x {:>8.2}x {:>8.2}x {bar:>7.2}x {:>8.2}x  {}",
            geometric_mean(&members),
            low,
            high,
            worst,
            inillucent_compat::quiet::verdict(graded, met)
        );
        let _ = &rolled;
    }

    met_every_family
}

/// @param placement - the processors this process was pinned to
fn run(fixture: &Path, settings: &Settings, placement: &Placement) -> Result<Option<bool>, String> {
    let bench = sqlite_bench().ok_or_else(|| {
        "sqlite-bench is not built; run tools/sqlite-reference.ps1 first".to_string()
    })?;

    let mut plan = plan_for(&settings.scale);
    plan.setup.clear();
    plan.workloads
        .retain(|workload| settings.families.contains(&workload.family));
    if let Some(repeat) = settings.repeat_override {
        for workload in &mut plan.workloads {
            workload.repeat = repeat;
        }
    }
    if plan.workloads.is_empty() {
        return Err(format!(
            "no workload is in the families {:?}",
            settings.families
        ));
    }
    let pool_bytes = settings.frames.saturating_mul(settings.page_size);
    plan.cache_size = -((pool_bytes / 1024) as i32);

    print_configuration(settings, &plan, pool_bytes, placement);
    println!("## workloads");
    for workload in &plan.workloads {
        println!(
            "  {:<24} {:<12} repeat {:<7} grouping {}",
            workload.name,
            workload.family,
            workload.repeat,
            workload.grouping.name()
        );
    }

    let plan_path =
        std::env::temp_dir().join(format!("inillucent-writegate-{}.plan", std::process::id()));
    std::fs::write(&plan_path, plan.render())
        .map_err(|error| format!("could not write the plan: {error}"))?;
    let scratch = std::env::temp_dir().join(format!("inillucent-writegate-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)
        .map_err(|error| format!("could not make a scratch directory: {error}"))?;

    let mut measured: Vec<Paired> = plan
        .workloads
        .iter()
        .map(|workload| Paired {
            workload: workload.name.clone(),
            family: workload.family.clone(),
            pairs: Vec::with_capacity(settings.rounds as usize),
            agreed: true,
            disagreement: String::new(),
        })
        .collect();
    let mut refused: Vec<(String, String)> = Vec::new();

    print_where_one_statement_goes(fixture, &scratch, &plan, settings)?;

    println!();
    println!("## {} paired rounds, interleaved", settings.rounds);
    let started = Instant::now();
    for round in 0..settings.rounds {
        // The engine order alternates by round so a warm cache or a busy machine
        // does not systematically favour whichever went first.
        let ours_first = round % 2 == 0;
        let ((ours, our_state), (theirs, their_state)) = if ours_first {
            let ours = time_new_engine(fixture, &scratch, &plan.workloads, &plan, settings)?;
            let theirs = time_sqlite(&bench, &plan_path, fixture, &scratch)?;
            (ours, theirs)
        } else {
            let theirs = time_sqlite(&bench, &plan_path, fixture, &scratch)?;
            let ours = time_new_engine(fixture, &scratch, &plan.workloads, &plan, settings)?;
            (ours, theirs)
        };
        // **The clock is read only after the two engines agree about the data.**
        // A round whose arms wrote different rows is not a slow round, it is a
        // wrong one, and timing it would put a correctness failure into a
        // performance number.
        if our_state != their_state {
            for entry in &mut measured {
                entry.agreed = false;
                entry.disagreement = format!(
                    "round {round}: inillucent {our_state:?} against sqlite {their_state:?}"
                );
            }
            break;
        }
        for (index, workload) in plan.workloads.iter().enumerate() {
            let Some(slot) = measured.get_mut(index) else {
                continue;
            };
            let mine = ours.iter().find(|sample| sample.workload == workload.name);
            let Some(mine) = mine else {
                if round == 0 {
                    refused.push((
                        workload.name.clone(),
                        "the new engine produced no sample".to_string(),
                    ));
                }
                slot.agreed = false;
                slot.disagreement = "the new engine produced no sample".to_string();
                continue;
            };
            let Some(reference) = theirs
                .iter()
                .find(|sample| sample.workload == workload.name)
            else {
                return Err(format!("{}: sqlite produced no sample", workload.name));
            };
            slot.pairs.push((mine.nanos, reference.nanos));
        }
        if round == 0 {
            println!(
                "  round 0 took {:.1}s including both restores",
                started.elapsed().as_secs_f64()
            );
        }
    }

    // **Before any verdict is printed (task-2110).** The bound was measured on
    // the read workloads, and a write's time also moves with the disk, so the
    // check here says so rather than claiming the same precision.
    let graded = inillucent_compat::quiet::check(&measured, &plan, &settings.quiet).graded();
    println!("  (the 3% bound was measured on the full gate's read workloads; a write also waits on the disk)");

    println!();
    println!("## result");
    println!(
        "  {:<24} {:>14} {:>14} {:>9} {:>9} {:>9}  agreed",
        "workload", "inillucent ns", "sqlite ns", "ratio", "low", "high"
    );
    let mut passed = true;
    for entry in &measured {
        if !entry.agreed || entry.pairs.is_empty() {
            println!(
                "  {:<24} {:>14} {:>14} {:>9} {:>9} {:>9}  NO: {}",
                entry.workload, "-", "-", "-", "-", "-", entry.disagreement
            );
            passed = false;
            continue;
        }
        let (ours, theirs) = entry.medians();
        let (low, high) = entry.interval(SEED);
        println!(
            "  {:<24} {:>14.0} {:>14.0} {:>8.2}x {:>8.2}x {:>8.2}x  yes",
            entry.workload,
            ours,
            theirs,
            entry.ratio(),
            low,
            high
        );
    }

    passed = passed && report_families(settings, &measured, graded);
    println!();
    println!(
        "## gate: {}",
        inillucent_compat::quiet::gate_line(graded, passed)
    );
    Ok(graded.then_some(passed))
}

/// Prints where one statement's time goes, before any of it is compared to
/// anything.
///
/// The gate misses on the overhead of a single statement, and a guess about
/// which half of a statement is expensive is a guess this project has been
/// wrong about before. So the report opens with a measurement of one statement
/// on its own: how long finding the row takes, how long applying the change
/// takes, and how many rows each statement inserted, deleted, updated in place
/// or compacted for.
///
/// **Lifted out of [`run`] (task-2029), for the reason
/// [`print_configuration`] was.** `run` was recorded at 226 lines and running
/// the workloads' `pre` here pushed it to 237, and the ratchet in `policy.rs`
/// says to split one out rather than raise the number. This is a stage: it
/// measures, it prints, and it decides nothing the rest of the gate reads.
///
/// @param fixture - the pristine database each workload is profiled against
/// @param scratch - where the per workload copy goes
/// @param plan - the workloads to profile, and the row count their binds use
/// @param settings - the page size and pool size the copies are opened with
fn print_where_one_statement_goes(
    fixture: &Path,
    scratch: &Path,
    plan: &inillucent_compat::perf::Plan,
    settings: &Settings,
) -> Result<(), String> {
    println!();
    println!("## where one statement goes, microseconds");
    println!(
        "  {:<24} {:>9} {:>9} {:>9} {:>8} {:>8} {:>8} {:>8}",
        "workload", "find", "apply", "total", "ins", "del", "inplace", "compact"
    );
    // **A fresh database per workload, because the gate gives each one a
    // fresh database.** All of them used to run against one copy, in list
    // order, and the numbers that came out described a file the earlier
    // workloads had already rewritten. On the `transaction` family that was
    // not a small distortion: `txn.autocommit`, `txn.batched` and
    // `txn.large` bind the same scattered rowids and the same
    // `row {iteration} lorem ipsum ...` text, so by the time `txn.large`
    // ran, every row it touched already held the exact bytes it was about
    // to write. Nothing differed, the in-place path is only reached when
    // exactly one column does, and the profile reported `inplace 0.00` for
    // a workload that in the gate takes that path on every statement.
    for workload in &plan.workloads {
        let copy = restore(fixture, scratch, "profile")?;
        let mut database = ImportedDatabase::import_with(copy, settings.page_size, settings.frames)
            .map_err(|error| format!("import failed: {}", why(&error)))?;
        // **The profile runs the workload's `pre` too, for the same reason
        // the gate does.** This block exists to say which half of a
        // statement is expensive, and a `txn` statement that rewrites the
        // bytes already in the row takes a different path from one that
        // changes a value - the update that happens in place. Profiling it
        // without the reset described which path a round *used to* take, not
        // the one it takes now.
        if let Err(reason) = run_setup(&mut database, workload.pre.as_deref()) {
            eprintln!("  {}: pre refused: {reason}", workload.name);
            continue;
        }
        let Ok(statement) = database.prepare_statement(&workload.sql) else {
            continue;
        };
        // **The workload's own repeat, not a sample of it.** Two hundred of
        // `txn.large`'s two thousand statements never fill a delta area, so
        // the compaction column read 0.000 for the workload whose
        // compactions are the reason it is on this page at all. The probe
        // now runs exactly what one gate round runs.
        let iterations = workload.repeat.max(1);
        let before = database.write_stats();
        database.begin_batch();
        let mut find = 0u128;
        let mut apply = 0u128;
        for iteration in 0..iterations {
            let params = Params::from_values(
                workload
                    .binds
                    .iter()
                    .map(|bind| bind_value(*bind, iteration, plan.rows))
                    .collect(),
            );
            match database.execute_timed(&statement, &params) {
                Ok((one, two)) => {
                    find = find.saturating_add(one);
                    apply = apply.saturating_add(two);
                }
                Err(_) => break,
            }
        }
        let _ = database.commit_batch();
        let each = u128::from(iterations).max(1);
        let after = database.write_stats();
        let per =
            |now: u64, was: u64| (now.saturating_sub(was) as f64) / (iterations.max(1) as f64);
        println!(
            "  {:<24} {:>9.2} {:>9.2} {:>9.2} {:>8.2} {:>8.2} {:>8.2} {:>8.3}",
            workload.name,
            (find / each) as f64 / 1000.0,
            (apply / each) as f64 / 1000.0,
            ((find + apply) / each) as f64 / 1000.0,
            per(after.inserted, before.inserted),
            per(after.deleted, before.deleted),
            per(after.updated_in_place, before.updated_in_place),
            per(after.compactions, before.compactions),
        );
    }
    Ok(())
}

/// Returns the geometric mean of a family's per-workload ratios.
///
/// Each workload counts once whatever its absolute time, and the geometric mean
/// is the right centre for a ratio - halving and doubling are the same size of
/// change.
///
/// @param members - the workloads in the family
fn geometric_mean(members: &[&Paired]) -> f64 {
    let logs: Vec<f64> = members
        .iter()
        .map(|entry| entry.ratio().max(f64::MIN_POSITIVE).ln())
        .collect();
    if logs.is_empty() {
        return 0.0;
    }
    (logs.iter().sum::<f64>() / logs.len() as f64).exp()
}

/// Returns the family's bootstrap interval, one per-round mean per round.
///
/// The statistic is `perf::family_interval`, shared with every other gate and
/// the scorecard so that no two of them grade a family by different numbers.
///
/// @param members - the workloads in the family
/// @param seed - the seed the resampling uses
fn family_bounds(members: &[&Paired], seed: u64) -> (f64, f64) {
    let (_, low, high) = inillucent_compat::perf::family_interval(members, seed);
    (low, high)
}

/// Returns a fresh copy of the fixture for one arm of one round.
///
/// The copy is made outside every timed region. A write workload changes what it
/// measures, so a round that ran on the previous round's leftovers would be
/// measuring a bigger table with a deeper tree - and the two arms would diverge
/// from each other as well as from themselves.
///
/// @param fixture - the pristine database
/// @param scratch - where the copy goes
/// @param tag - what to name it
fn restore(fixture: &Path, scratch: &Path, tag: &str) -> Result<PathBuf, String> {
    let target = scratch.join(format!("{tag}.db"));
    let _ = std::fs::remove_file(&target);
    std::fs::copy(fixture, &target)
        .map_err(|error| format!("could not restore the fixture: {error}"))?;
    Ok(target)
}

/// Times every workload on the new engine, over a fresh copy.
///
/// @param fixture - the pristine database
/// @param scratch - where the copy goes
/// @param workloads - what to run
/// @param plan - the plan, for its row count
/// @param settings - the page size and pool size
fn time_new_engine(
    fixture: &Path,
    scratch: &Path,
    workloads: &[Workload],
    plan: &inillucent_compat::perf::Plan,
    settings: &Settings,
) -> Result<(Vec<Sample>, Vec<String>), String> {
    let copy = restore(fixture, scratch, "ours")?;
    let mut database = ImportedDatabase::import_with(copy, settings.page_size, settings.frames)
        .map_err(|error| format!("import failed: {}", why(&error)))?;
    let mut samples = Vec::with_capacity(workloads.len());
    for workload in workloads {
        match time_one(&mut database, workload, plan.rows) {
            Ok(sample) => samples.push(sample),
            // A workload the engine refuses is *absent* rather than zero. A
            // sample of zero would roll into its family's ratio as an infinitely
            // fast write, which is the shape of lie a gate exists to prevent.
            Err(reason) => eprintln!("  {}: refused: {reason}", workload.name),
        }
    }
    let mut state = Vec::with_capacity(AGREEMENT.len());
    for question in AGREEMENT {
        let answer = database
            .execute_any(question, &Params::new())
            .map_err(|error| format!("{question}: {}", why(&error)))?;
        state.push(render_row(&answer.rows));
    }
    Ok((samples, state))
}

/// Renders one answer as the text the two arms are compared by.
///
/// @param rows - the answer
fn render_row(rows: &[Vec<OwnedDatum>]) -> String {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    OwnedDatum::Null => "null".to_string(),
                    OwnedDatum::Int(number) => number.to_string(),
                    OwnedDatum::Real(number) => format!("{number}"),
                    OwnedDatum::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                    OwnedDatum::Blob(bytes) => format!("blob:{}", bytes.len()),
                })
                .collect::<Vec<String>>()
                .join("|")
        })
        .collect::<Vec<String>>()
        .join(";")
}

/// Runs a workload's `pre` script, when it has one, outside every timed region.
///
/// Setup, not work: the statements here are what puts the table back into the
/// state the workload is meant to start from, and `sqlite_bench.c` runs them
/// before it reads its own clock. A gate arm that skips them is measuring a
/// different database from the arm that does not, and the questions asked at
/// the end of a round are what notice.
///
/// Split on `;` the way `run_batch` in `fullgate.rs` splits, because a `pre` is
/// a script rather than a statement and the engine's `execute_any` takes one
/// statement at a time.
///
/// @param database - the imported fixture, opened for writing
/// @param script - the workload's `pre`, when it has one
fn run_setup(database: &mut ImportedDatabase, script: Option<&str>) -> Result<(), String> {
    let Some(script) = script else {
        return Ok(());
    };
    for statement in script.split(';') {
        let trimmed = statement.trim();
        if trimmed.is_empty() {
            continue;
        }
        database
            .execute_any(trimmed, &Params::new())
            .map_err(|error| format!("{trimmed}: {}", why(&error)))?;
    }
    Ok(())
}

/// Times one workload and digests the table it changed.
///
/// @param database - the imported fixture, opened for writing
/// @param workload - what to run
/// @param rows - how many rows the base table holds
fn time_one(
    database: &mut ImportedDatabase,
    workload: &Workload,
    rows: u32,
) -> Result<Sample, String> {
    // **`pre` runs here, before the clock, because the other arm runs it.**
    // `sqlite_bench.c` executes a workload's `pre` at line 321, ahead of
    // `sqlite3_prepare_v2` and ahead of the timed loop. This gate used to skip
    // it, and that was the whole of task-2029: `txn.batched` and `txn.large`
    // both carry `UPDATE side_table SET note = 'note ' || id`, so SQLite reset
    // every note twice per round and inillucent never did. The agreement
    // question asked at the end of a round, `sum(length(note))`, then read
    // 261865 against 258445 - a gap of 3420 that the gate correctly refused to
    // time, on every workload, on every fixture.
    //
    // It also meant the two arms were not measuring the same work. The three
    // `transaction` workloads bind the same scattered ids and the same text, so
    // without the reset `txn.large` was writing back the bytes `txn.batched`
    // had already written - 4,067 ns against 1,723 ns for an update that
    // changes a value, which is the measurement that put the `pre` in
    // `perf.rs` in the first place.
    run_setup(database, workload.pre.as_deref())?;
    // The statement is bound once per iteration because its parameters change,
    // which is what SQLite's arm does too: it resets, re-binds and steps a
    // program compiled once. What must not differ is the *work*, and the work
    // is one statement per iteration on both sides.
    // The compile happens **before the clock starts**, which is where SQLite's
    // `sqlite3_prepare_v2` already is. What is inside the timed region on both
    // arms is the same thing: bind, step, and the transaction boundaries.
    let statement = database
        .prepare_statement(&workload.sql)
        .map_err(|error| why(&error))?;
    let mut changed = 0u64;
    let started = Instant::now();
    if workload.grouping != Grouping::Autocommit {
        database.begin_batch();
    }
    for iteration in 0..workload.repeat {
        let params = Params::from_values(
            workload
                .binds
                .iter()
                .map(|bind| bind_value(*bind, iteration, rows))
                .collect(),
        );
        let outcome = database
            .execute_statement(&statement, &params)
            .map_err(|error| why(&error))?;
        changed = changed.saturating_add(outcome.changes.rows as u64);
        // The grouping decides where the commits are, and the rule is
        // `sqlite_bench.c`'s, clause for clause: commit on the boundary, and
        // open the next transaction only if there is another iteration to put
        // in it. Opening one anyway left an empty transaction to be committed
        // at the end - one commit record and one fsync the other arm does not
        // pay.
        if let Grouping::Every(every) = workload.grouping {
            if every > 0 && iteration.saturating_add(1) % every == 0 {
                database.commit_batch().map_err(|error| why(&error))?;
                if iteration.saturating_add(1) < workload.repeat {
                    database.begin_batch();
                }
            }
        }
    }
    database.commit_batch().map_err(|error| why(&error))?;
    let nanos = started.elapsed().as_nanos() as f64;
    Ok(Sample {
        workload: workload.name.clone(),
        nanos,
        rows: changed,
        digest: 0,
    })
}

/// Times every workload on SQLite, over its own fresh copy.
///
/// @param bench - the pinned driver
/// @param plan - the plan file
/// @param fixture - the pristine database
/// @param scratch - where the copy goes
fn time_sqlite(
    bench: &Path,
    plan: &Path,
    fixture: &Path,
    scratch: &Path,
) -> Result<(Vec<Sample>, Vec<String>), String> {
    let copy = restore(fixture, scratch, "theirs")?;
    // Started through the affinity check (task-2085): the launcher reads the
    // child's mask back and refuses a reference arm on other processors.
    let output = affinity::spawn_on_same_cores(
        Command::new(bench)
            .arg("run")
            .arg(plan)
            .arg(&copy)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()),
        "sqlite-bench",
    )?
    .wait_with_output()
    .map_err(|error| format!("sqlite-bench did not finish: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "sqlite-bench failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let samples: Vec<Sample> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(Sample::parse)
        .collect();
    let mut state = Vec::with_capacity(AGREEMENT.len());
    for question in AGREEMENT {
        state.push(ask_sqlite(&copy, question)?);
    }
    Ok((samples, state))
}

/// Asks SQLite one question about the database it just wrote.
///
/// @param database - the copy SQLite wrote to
/// @param sql - the question
fn ask_sqlite(database: &Path, sql: &str) -> Result<String, String> {
    let shell = workspace_root().join(".sqlite-ref/3.53.4/shell/sqlite3.exe");
    let shell = if shell.exists() {
        shell
    } else {
        workspace_root().join(".sqlite-ref/3.53.4/shell/sqlite3")
    };
    let output = Command::new(shell)
        .arg(database)
        .arg(format!("{sql};"))
        .output()
        .map_err(|error| format!("the sqlite shell did not start: {error}"))?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<&str>>()
        .join(";"))
}

/// Returns the `sqlite-bench` executable, if it has been built.
fn sqlite_bench() -> Option<PathBuf> {
    let root = workspace_root().join(".sqlite-ref/3.53.4");
    for name in ["sqlite-bench.exe", "sqlite-bench"] {
        let path = root.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}
