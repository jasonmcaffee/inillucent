//! The Phase 4 gate: **the whole scorecard plan** on the new engine, against
//! pinned SQLite.
//!
//! Invariant: every workload in the plan is run, digest-compared, and timed -
//! not a family list, the whole thing. The Phase 4 acceptance says so in as many
//! words: *"the full scorecard runs with every workload digest-equal - not the
//! read gate, the whole thing"*, and until this binary existed there was nothing
//! that sentence could be true of. `readgate` covers four read families and
//! `writegate` two write ones; between them they leave `open.prepare`, `schema`,
//! `extension` and `large.values` unmeasured on the new engine.
//!
//! ## What it takes from each of the two gates it replaces
//!
//! **From `writegate`: a fresh fixture per arm per round.** A mutating workload
//! changes what it measures, so the second round of an unrestored fixture is
//! measuring a different table. Every round hands each arm its own copy, made
//! outside every timed region.
//!
//! **From `readgate`: a digesting sink rather than a result set.** SQLite's arm
//! digests inside its `sqlite3_step` loop and keeps nothing, so an arm that
//! built a `Vec<Vec<OwnedDatum>>` first would be timing a result set neither
//! engine's caller asked for. Non-mutating workloads therefore run through the
//! pipeline with the same `DigestRows` sink the read gate uses.
//!
//! ## The three things the plan asks for that neither gate did
//!
//! **`pre` and `post`, outside the clock.** `sqlite_bench.c` runs them either
//! side of the timed region, so `schema.index`'s `DROP INDEX IF EXISTS` and
//! `extension.fts.build`'s `CREATE VIRTUAL TABLE` are setup rather than work.
//! An arm that timed them would be timing a different statement.
//!
//! **`prepare` per iteration, inside the clock.** For a workload the plan marks
//! `prepare: each` - `open.prepare`'s two, and `schema.index` - SQLite calls
//! `sqlite3_prepare_v2` *inside* the loop. So does this. That is the whole of
//! what the `open.prepare` family measures, and a harness that hoisted the
//! compile out would be measuring nothing.
//!
//! **`schema.index`, which is DDL.** It compiles and runs a `CREATE INDEX`
//! inside the timed region, exactly as the SQLite arm does.
//!
//! Usage:
//!   inillucent-fullgate <sqlite fixture> [--rounds N] [--page-size N]
//!                       [--scale S] [--frames N] [--families a,b] [--repeat N]

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::Instant;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::perf::{plan_for, Bind, Digest, Grouping, Paired, Sample, Workload};
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// The seed the bootstrap resamples with, so an interval is reproducible.
const SEED: u64 = 0x5eed_1833;

/// Every family, with the bar the TDD sets for it.
///
/// The three Phase 4 owns are quoted from this ticket - `schema` at least 3.0x,
/// `extension` at least 1.5x, `large.values` at least 1.5x. The rest are the
/// bars the earlier phases were measured against and are carried so that a
/// Phase 4 change that cost a read family shows up here rather than in Phase 5.
/// `open.prepare` takes the TDD's own low estimate of 5x.
const FAMILIES: [(&str, f64); 10] = [
    ("open.prepare", 5.0),
    ("read.point", 2.0),
    ("read.range", 3.0),
    ("read.join", 3.0),
    ("read.analytical", 5.0),
    ("write", 1.5),
    ("transaction", 1.0),
    ("schema", 3.0),
    ("extension", 1.5),
    ("large.values", 1.5),
];

/// What the gate was asked to measure.
struct Settings {
    rounds: u32,
    page_size: usize,
    frames: usize,
    scale: String,
    families: Vec<String>,
    repeat_override: Option<u32>,
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(fixture) = arguments.first().filter(|first| !first.starts_with("--")) else {
        eprintln!(
            "usage: inillucent-fullgate <sqlite fixture> [--rounds N] [--page-size N] \
             [--scale S] [--frames N] [--families a,b] [--repeat N]"
        );
        return ExitCode::from(2);
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
    };
    match run(Path::new(fixture), &settings) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(reason) => {
            eprintln!("full gate: {reason}");
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
fn run(fixture: &Path, settings: &Settings) -> Result<bool, String> {
    let bench = sqlite_bench().ok_or_else(|| {
        "sqlite-bench is not built; run tools/sqlite-reference.ps1 first".to_string()
    })?;

    let mut plan = plan_for(&settings.scale);
    plan.setup.clear();
    plan.workloads.retain(|workload| {
        settings
            .families
            .iter()
            .any(|name| *name == workload.family)
    });
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

    println!("## configuration");
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
    println!(
        "  warm state  : inillucent's pool is filled before each round; SQLite's cache fills as the plan runs"
    );
    println!("  durability  : synchronous = FULL on both arms");
    println!();
    println!("## workloads");
    for workload in &plan.workloads {
        println!(
            "  {:<24} {:<16} repeat {:<7} grouping {:<10} prepare {}",
            workload.name,
            workload.family,
            workload.repeat,
            workload.grouping.name(),
            if workload.prepare_each {
                "each"
            } else {
                "once"
            }
        );
    }

    let plan_path =
        std::env::temp_dir().join(format!("inillucent-fullgate-{}.plan", std::process::id()));
    std::fs::write(&plan_path, plan.render())
        .map_err(|error| format!("could not write the plan: {error}"))?;
    let scratch = std::env::temp_dir().join(format!("inillucent-fullgate-{}", std::process::id()));
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

    println!();
    println!("## {} paired rounds, interleaved", settings.rounds);
    let started = Instant::now();
    for round in 0..settings.rounds {
        // The engine order alternates by round so a warm cache or a busy machine
        // does not systematically favour whichever went first.
        let ours_first = round % 2 == 0;
        let ((ours, our_state), (theirs, their_state)) = if ours_first {
            let ours = time_new_engine(fixture, &scratch, &plan, settings)?;
            let theirs = time_sqlite(&bench, &plan_path, fixture, &scratch)?;
            (ours, theirs)
        } else {
            let theirs = time_sqlite(&bench, &plan_path, fixture, &scratch)?;
            let ours = time_new_engine(fixture, &scratch, &plan, settings)?;
            (ours, theirs)
        };
        // **The clock is read only after the two engines agree about the data.**
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
            let Some(mine) = ours.iter().find(|sample| sample.workload == workload.name) else {
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
            // A row-producing workload is compared by its digest. A write
            // produces no rows on either arm, so comparing the digests of two
            // empty result sets proves nothing - what those are compared by is
            // the state questions above, at the end of the round.
            if !workload.mutates && (mine.digest != reference.digest || mine.rows != reference.rows)
            {
                slot.agreed = false;
                slot.disagreement = format!(
                    "inillucent {} rows digest {:016x} against sqlite {} rows digest {:016x}",
                    mine.rows, mine.digest, reference.rows, reference.digest
                );
                continue;
            }
            slot.pairs.push((mine.nanos, reference.nanos));
        }
        if round == 0 {
            println!(
                "  round 0 took {:.1}s including both restores",
                started.elapsed().as_secs_f64()
            );
        }
    }

    println!();
    println!("## result");
    println!(
        "  {:<24} {:>14} {:>14} {:>9} {:>9} {:>9}  {}",
        "workload", "inillucent ns", "sqlite ns", "ratio", "low", "high", "agreed"
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

    println!();
    println!("## families");
    println!(
        "  {:<16} {:>9} {:>9} {:>9} {:>8} {:>9}  {}",
        "family", "ratio", "low", "high", "bar", "worst", "verdict"
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
                "  {family:<16} {:>9} {:>9} {:>9} {bar:>7.2}x  NO DATA",
                "-", "-", "-"
            );
            passed = false;
            continue;
        }
        // The family is one log ratio per workload per round, weighted equally
        // per workload - `writegate`'s rollup, which task-1832 arrived at after
        // two wrong ones. Pooling raw pairs lets a workload with a hundred times
        // the absolute time decide the family alone; collapsing each workload to
        // its median first makes a three-point bootstrap whose lower bound *is*
        // the minimum.
        let (low, high) = pooled_interval(&members, SEED);
        let worst = members
            .iter()
            .map(|entry| entry.ratio())
            .fold(f64::INFINITY, f64::min);
        // **The lower bound against the bar, not the point estimate.**
        let met = low >= bar;
        passed = passed && met;
        println!(
            "  {family:<16} {:>8.2}x {:>8.2}x {:>8.2}x {bar:>7.2}x {:>8.2}x  {}",
            geometric_mean(&members),
            low,
            high,
            worst,
            if met { "MET" } else { "MISSED" }
        );
    }

    println!();
    println!("## gate: {}", if passed { "MET" } else { "NOT MET" });
    Ok(passed)
}

/// Returns the geometric mean of a family's per-workload ratios.
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

/// Returns the family's bootstrap interval over every workload's every round.
///
/// @param members - the workloads in the family
/// @param seed - the seed the resampling uses
fn pooled_interval(members: &[&Paired], seed: u64) -> (f64, f64) {
    let logs: Vec<f64> = members
        .iter()
        .flat_map(|entry| entry.log_ratios())
        .collect();
    let (low, high) = inillucent_compat::perf::bootstrap(&logs, seed);
    (low.exp(), high.exp())
}

/// Returns a fresh copy of the fixture for one arm of one round.
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
/// @param plan - the plan, for its workloads and row count
/// @param settings - the page size and pool size
fn time_new_engine(
    fixture: &Path,
    scratch: &Path,
    plan: &inillucent_compat::perf::Plan,
    settings: &Settings,
) -> Result<(Vec<Sample>, Vec<String>), String> {
    let copy = restore(fixture, scratch, "ours")?;
    let mut database = ImportedDatabase::import_with(copy, settings.page_size, settings.frames)
        .map_err(|error| format!("import failed: {}", why(&error)))?;
    // **The pool is filled before the clock starts, which is what the read gate
    // does and what makes these numbers comparable to Phase 2's and Phase 3's.**
    // It is an asymmetry and the configuration says so: SQLite's arm has no
    // equivalent hook, and its own cache fills as the plan's earlier workloads
    // read the same tables. The import closes and reopens the file, so without
    // this the first workload of every round would be paying for the whole
    // fixture's first read.
    database
        .warm()
        .map_err(|error| format!("warming failed: {}", why(&error)))?;
    let mut samples = Vec::with_capacity(plan.workloads.len());
    for workload in &plan.workloads {
        // `pre` and `post` are setup, not work: `sqlite_bench.c` runs them
        // either side of the timed region and so does this.
        if let Some(pre) = &workload.pre {
            if let Err(reason) = run_batch(&mut database, pre) {
                eprintln!("  {}: pre refused: {reason}", workload.name);
                continue;
            }
        }
        let timed = if workload.mutates {
            time_write(&mut database, workload, plan.rows)
        } else {
            time_read(&database, workload, plan.rows)
        };
        match timed {
            Ok(sample) => samples.push(sample),
            // A workload the engine refuses is *absent* rather than zero. A
            // sample of zero would roll into its family as an infinitely fast
            // one, which is the shape of lie a gate exists to prevent.
            Err(reason) => eprintln!("  {}: refused: {reason}", workload.name),
        }
        if let Some(post) = &workload.post {
            if let Err(reason) = run_batch(&mut database, post) {
                eprintln!("  {}: post refused: {reason}", workload.name);
            }
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

/// Runs a semicolon-separated setup script.
///
/// @param database - the imported fixture
/// @param script - the statements
fn run_batch(database: &mut ImportedDatabase, script: &str) -> Result<(), String> {
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

/// Times one non-mutating workload, digesting its rows as they are produced.
///
/// The sink is the read gate's: SQLite digests inside its step loop and keeps
/// nothing, so an arm that materialised the result set first would be timing a
/// result neither engine's caller asked for.
///
/// @param database - the imported fixture
/// @param workload - what to run
/// @param rows - how many rows the base table holds
fn time_read(
    database: &ImportedDatabase,
    workload: &Workload,
    rows: u32,
) -> Result<Sample, String> {
    let folded = Rc::new(RefCell::new(Folded::default()));
    if workload.prepare_each {
        // The compile is inside the clock, which is where SQLite's is for a
        // workload the plan marks `prepare: each` - and for `open.prepare` it is
        // the whole of what is being measured.
        let started = Instant::now();
        for iteration in 0..workload.repeat {
            let params = params_for(workload, iteration, rows);
            let plan = database.plan(&workload.sql).map_err(|error| why(&error))?;
            let choice = database.prepare(&plan).map_err(|error| why(&error))?;
            let sink = Box::new(DigestRows {
                folded: Rc::clone(&folded),
            });
            let (mut pipeline, _) = database
                .pipeline(&plan, &choice, &params, sink)
                .map_err(|error| why(&error))?;
            pipeline.run().map_err(|error| why(&error))?;
        }
        let nanos = started.elapsed().as_secs_f64() * 1e9;
        let held = folded.replace(Folded::default());
        return Ok(Sample {
            workload: workload.name.clone(),
            nanos,
            rows: held.rows,
            digest: held.digest.finish(),
        });
    }
    let plan = database.plan(&workload.sql).map_err(|error| why(&error))?;
    let choice = database.prepare(&plan).map_err(|error| why(&error))?;
    let sink = Box::new(DigestRows {
        folded: Rc::clone(&folded),
    });
    let mut statement = database
        .statement(&plan, &choice, &params_for(workload, 0, rows), sink)
        .map_err(|error| why(&error))?;
    if !statement.rebindable() {
        return Err("the operator chain folded a bound parameter in".to_string());
    }
    let mut params = Params::from_values(Vec::new());
    let started = Instant::now();
    for iteration in 0..workload.repeat {
        params.refill(
            workload
                .binds
                .iter()
                .map(|bind| bind_value(*bind, iteration, rows)),
        );
        statement.run(&params).map_err(|error| why(&error))?;
    }
    let nanos = started.elapsed().as_secs_f64() * 1e9;
    let held = folded.replace(Folded::default());
    Ok(Sample {
        workload: workload.name.clone(),
        nanos,
        rows: held.rows,
        digest: held.digest.finish(),
    })
}

/// Times one mutating workload.
///
/// @param database - the imported fixture, opened for writing
/// @param workload - what to run
/// @param rows - how many rows the base table holds
fn time_write(
    database: &mut ImportedDatabase,
    workload: &Workload,
    rows: u32,
) -> Result<Sample, String> {
    if workload.prepare_each {
        // A DDL statement, or any other the plan marks `prepare: each`. The
        // compile is inside the clock because SQLite's `sqlite3_prepare_v2` is.
        let started = Instant::now();
        for iteration in 0..workload.repeat {
            let params = params_for(workload, iteration, rows);
            database
                .execute_any(&workload.sql, &params)
                .map_err(|error| why(&error))?;
        }
        return Ok(Sample {
            workload: workload.name.clone(),
            nanos: started.elapsed().as_secs_f64() * 1e9,
            rows: 0,
            digest: 0,
        });
    }
    // The compile happens before the clock starts, which is where SQLite's
    // already is. What is inside the timed region on both arms is the same
    // thing: bind, step, and the transaction boundaries.
    let statement = database
        .prepare_statement(&workload.sql)
        .map_err(|error| why(&error))?;
    let mut changed = 0u64;
    let started = Instant::now();
    if workload.grouping != Grouping::Autocommit {
        database.begin_batch();
    }
    for iteration in 0..workload.repeat {
        let params = params_for(workload, iteration, rows);
        let outcome = database
            .execute_statement(&statement, &params)
            .map_err(|error| why(&error))?;
        changed = changed.saturating_add(outcome.changes.rows as u64);
        // The grouping decides where the commits are, and the rule is
        // `sqlite_bench.c`'s, clause for clause.
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
    Ok(Sample {
        workload: workload.name.clone(),
        nanos: started.elapsed().as_secs_f64() * 1e9,
        rows: changed,
        digest: 0,
    })
}

/// Returns the parameters one iteration binds.
///
/// @param workload - the workload
/// @param iteration - which iteration, from zero
/// @param rows - how many rows the base table holds
fn params_for(workload: &Workload, iteration: u32, rows: u32) -> Params {
    Params::from_values(
        workload
            .binds
            .iter()
            .map(|bind| bind_value(*bind, iteration, rows))
            .collect(),
    )
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

/// Returns the value one bind kind produces for one iteration.
///
/// The formulas are `compat/oracle/sqlite_bench.c`'s `bind_one`, transcribed -
/// the same transcription both other gates carry, and for the same reason: a
/// benchmark whose two arms read different rows is not a comparison.
///
/// @param bind - the bind kind
/// @param iteration - which iteration, from zero
/// @param rows - how many rows the base table holds
fn bind_value(bind: Bind, iteration: u32, rows: u32) -> OwnedDatum {
    let iteration = u64::from(iteration);
    let rows64 = u64::from(rows);
    match bind {
        Bind::Rowid => OwnedDatum::Int(if rows > 0 {
            1 + (iteration % rows64) as i64
        } else {
            1
        }),
        Bind::Scatter => OwnedDatum::Int(if rows > 0 {
            1 + (iteration.wrapping_mul(2_654_435_761) % rows64) as i64
        } else {
            1
        }),
        Bind::Counter => OwnedDatum::Int((rows64 + 1 + iteration) as i64),
        Bind::Int => OwnedDatum::Int(
            (iteration.wrapping_mul(1_103_515_245).wrapping_add(12_345) & 0x7fff_ffff) as i64,
        ),
        Bind::Text => OwnedDatum::Text(
            format!("row {iteration} lorem ipsum dolor sit amet consectetur").into_bytes(),
        ),
        Bind::Blob => {
            OwnedDatum::Blob((0..64u64).map(|j| ((iteration + j) & 0xff) as u8).collect())
        }
    }
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
    let output = Command::new(bench)
        .arg("run")
        .arg(plan)
        .arg(&copy)
        .output()
        .map_err(|error| format!("sqlite-bench did not start: {error}"))?;
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

/// What a digesting run accumulated, shared with the caller.
#[derive(Default)]
struct Folded {
    digest: Digest,
    rows: u64,
}

/// A sink that digests rows as they are produced and keeps none of them.
struct DigestRows {
    folded: Rc<RefCell<Folded>>,
}

impl inillucent_exec::Sink for DigestRows {
    fn push(
        &mut self,
        batch: &inillucent_exec::Batch<'_>,
    ) -> inillucent_base::DbResult<inillucent_exec::Flow> {
        let mut held = self.folded.borrow_mut();
        for nth in 0..batch.live() {
            for column in 0..batch.columns.len() {
                let value = batch.value(nth, column)?;
                eat_borrowed(&mut held.digest, &value);
            }
            held.rows = held.rows.saturating_add(1);
        }
        Ok(inillucent_exec::Flow::Continue)
    }

    fn finish(&mut self) -> inillucent_base::DbResult<()> {
        Ok(())
    }

    /// Returns the sink to its pre-input state; it keeps no rows to forget.
    fn reset(&mut self) -> inillucent_base::DbResult<()> {
        Ok(())
    }
}

/// Adds one borrowed value to the digest, tagged the way the reference tags it.
///
/// @param digest - the running digest
/// @param value - the value to fold in
fn eat_borrowed(digest: &mut Digest, value: &inillucent_tree::datum::Datum<'_>) {
    use inillucent_tree::datum::Datum;
    match value {
        Datum::Null => digest.tag(0),
        Datum::Int(number) => {
            digest.tag(1);
            digest.word(*number as u64);
        }
        Datum::Real(number) => {
            digest.tag(2);
            digest.word(number.to_bits());
        }
        Datum::Text(bytes) => {
            digest.tag(3);
            digest.word(bytes.len() as u64);
            digest.bytes(bytes);
        }
        Datum::Blob(bytes) => {
            digest.tag(4);
            digest.word(bytes.len() as u64);
            digest.bytes(bytes);
        }
    }
}
