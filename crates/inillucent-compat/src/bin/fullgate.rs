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

/// The engine's own allocator, installed for this program.
///
/// **Part of the build, not of a workload.** SQLite ships its own memory
/// subsystem and is compiled as one translation unit; a Rust workspace measured
/// on the platform allocator is being measured on a build configuration rather
/// than on an engine, which is the same reasoning that fixed fat LTO and one
/// codegen unit in the release profile. The Windows C runtime heap was
/// measured at 59% of a trivial compile and this size-classed free list at
/// 17% overall, which is why Phase 3's Part E names it the cheapest first move.
#[global_allocator]
static ALLOCATOR: inillucent_alloc::Pooled = inillucent_alloc::Pooled;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::Instant;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::perf::{
    plan_for, qualified_rounds, weighted_headline, Bind, Contract, Digest, Grouping, Paired,
    Sample, Workload,
};
use inillucent_compat::procstat::ProcessCost;
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
    /// The locking mode SQLite's arm runs in: `normal` or `exclusive`.
    locking: String,
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    // **A child of this same program, spawned by the parent for the memory
    // measurement and nothing else.** The reference arm is a whole child
    // process, so the only way to put this engine's residency beside it is to
    // make this engine a whole child process too - the parent's own peak holds
    // the harness, the plan and thirty rounds of both arms, and is not a number
    // about the engine at all.
    if let Some(database) = flag(&arguments, "--memory-round") {
        return match memory_round(Path::new(&database), &settings_from(&arguments)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(reason) => {
                eprintln!("memory round: {reason}");
                ExitCode::from(2)
            }
        };
    }
    let Some(fixture) = arguments.first().filter(|first| !first.starts_with("--")) else {
        eprintln!(
            "usage: inillucent-fullgate <sqlite fixture> [--rounds N] [--page-size N] \
             [--scale S] [--frames N] [--families a,b] [--repeat N] [--locking normal|exclusive]"
        );
        return ExitCode::from(2);
    };
    let settings = settings_from(&arguments);
    match run(Path::new(fixture), &settings) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(reason) => {
            eprintln!("full gate: {reason}");
            ExitCode::from(2)
        }
    }
}

/// Reads the settings off a command line.
///
/// @param arguments - the command line, without the program name
fn settings_from(arguments: &[String]) -> Settings {
    Settings {
        rounds: flag(arguments, "--rounds")
            .and_then(|value| value.parse().ok())
            .unwrap_or(30),
        // **32 KiB, because that is the page size this engine has.**
        // `Options::default` is 32 KiB, Phase 1 fixed it there after measuring
        // 16/32/64, and `readgate` - the harness every Phase 2 and Phase 3
        // number was taken with - defaults to it. This binary defaulted to
        // 8 KiB, a page size that was measured on purpose and rejected, so
        // every full-gate number from Phase 4 onwards was taken at a page
        // size the engine does not ship.
        //
        // The page size effect was isolated from the memory budget rather
        // than assumed, because the pool's bytes are frames times page size
        // and moving one moves the other. Three configurations, ten paired rounds, medium:
        //
        // | | 8 KiB / 32 MiB | 32 KiB / **32 MiB** | 32 KiB / 128 MiB |
        // |---|---|---|---|
        // | `range.lookaside` | 0.79x | 0.98x | 0.96x |
        // | `join.range` | 0.83x | 0.98x | 0.95x |
        // | `scan.sort` | 3.20x | 5.31x | 5.21x |
        // | `scan.distinct` | 1.04x | 1.63x | 1.60x |
        // | `read.analytical` | 4.25x | 5.92x | 5.77x |
        //
        // The middle column holds the budget at the first column's and moves
        // only the page size, and it reproduces the third. So the variable is
        // the page size and not the memory, and the cache stays matched either
        // way - SQLite's `cache_size` is derived from the pool's bytes below.
        page_size: flag(arguments, "--page-size")
            .and_then(|value| value.parse().ok())
            .unwrap_or(32_768),
        frames: flag(arguments, "--frames")
            .and_then(|value| value.parse().ok())
            .unwrap_or(4_096),
        scale: flag(arguments, "--scale").unwrap_or_else(|| "medium".to_string()),
        families: flag(arguments, "--families")
            .map(|value| value.split(',').map(str::to_string).collect())
            .unwrap_or_else(|| FAMILIES.iter().map(|(name, _)| name.to_string()).collect()),
        repeat_override: flag(arguments, "--repeat").and_then(|value| value.parse().ok()),
        locking: flag(arguments, "--locking").unwrap_or_else(|| "normal".to_string()),
    }
}

/// Runs one round of the plan in a child process, for the memory measurement.
///
/// The file is **opened**, not imported: the parent built it, so what this
/// process pays for is holding the data and running the plan, which is what the
/// reference child pays for too. Nothing is printed; the parent reads the
/// operating system's accounting for this process once it has exited.
///
/// @param database - the `.rdb` the parent built
/// @param settings - the page size, frame count, scale and family filter
fn memory_round(database: &Path, settings: &Settings) -> Result<(), String> {
    let plan = filtered_plan(settings)?;
    let mut opened =
        ImportedDatabase::open(database.to_path_buf(), settings.page_size, settings.frames)
            .map_err(|error| format!("open failed: {}", why(&error)))?;
    let (_, _, cost) = round_on(&mut opened, &plan)?;
    // **The only place a per-workload peak means anything.** This process runs
    // the plan once and nothing else, so its high-water mark is the engine's;
    // the parent's is the harness's. The parent reads these lines back off the
    // child's standard output and prints them, which is why they carry a tag
    // rather than being formatted here.
    for (name, peak, resident, frames) in &cost.marks {
        println!("mark\t{name}\t{peak}\t{resident}\t{frames}");
    }
    Ok(())
}

/// Returns the plan for the scale, filtered to the families asked for.
///
/// One function, because the parent and the memory child have to run the same
/// workloads in the same order or the child's number is about something else.
///
/// @param settings - the scale, the family filter and any repeat override
fn filtered_plan(settings: &Settings) -> Result<inillucent_compat::perf::Plan, String> {
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
    Ok(plan)
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

    let mut plan = filtered_plan(settings)?;
    plan.locking.clone_from(&settings.locking);
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
        "  sqlite lock : locking_mode = {} (this engine takes no file lock at all)",
        settings.locking
    );
    println!("  plan cache  : declared, and NOT used by either arm of this gate");
    println!(
        "                inillucent keeps a prepared plan per statement text, and the TDD names"
    );
    println!(
        "                it as the thing a reader is most likely to contest. This harness does"
    );
    println!("                not reach it: a prepare-each workload calls plan() and prepare()");
    println!("                inside the clock, and plan() parses, binds and plans on every call;");
    println!("                every other workload prepares once, outside the clock, and rebinds.");
    println!("                So no number here is helped by the cache, and SQLite compiles per");
    println!("                iteration for a prepare-each workload exactly as this does.");
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
    let mut our_rounds: Vec<RoundCost> = Vec::with_capacity(settings.rounds as usize);
    let mut their_rounds: Vec<ProcessCost> = Vec::with_capacity(settings.rounds as usize);
    for round in 0..settings.rounds {
        // The engine order alternates by round so a warm cache or a busy machine
        // does not systematically favour whichever went first.
        let ours_first = round % 2 == 0;
        let ((ours, our_state, our_cost), (theirs, their_state, their_cost)) = if ours_first {
            let ours = time_new_engine(fixture, &scratch, &plan, settings)?;
            let theirs = time_sqlite(&bench, &plan_path, fixture, &scratch)?;
            (ours, theirs)
        } else {
            let theirs = time_sqlite(&bench, &plan_path, fixture, &scratch)?;
            let ours = time_new_engine(fixture, &scratch, &plan, settings)?;
            (ours, theirs)
        };
        our_rounds.push(our_cost);
        their_rounds.push(their_cost);
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

    // Once, not per round: it is a residency measurement, and thirty of them
    // would say the same thing thirty times for the price of another gate.
    let child = measure_in_a_child(fixture, &scratch, settings);
    report_costs(
        &plan,
        &our_rounds,
        &their_rounds,
        child.as_ref(),
        settings.page_size,
    );

    println!();
    println!("## result");
    println!(
        "  {:<24} {:>14} {:>14} {:>9} {:>9} {:>9}  agreed",
        "workload", "inillucent ns", "sqlite ns", "ratio", "low", "high"
    );
    let mut passed = true;
    // **A family that produced no sample must not be renormalised away.**
    // `weighted_mean` averages over the families a round actually has and
    // divides by the weight it used, so a workload that fails outright makes
    // the headline go *up*: at large, `schema.index` could not run at all and
    // the headline read 4.16x; with the same runs and `schema` present at
    // 0.58x it reads 3.86x. A number that improves when a family breaks is not
    // a headline, and this is what stops it being printed as one.
    let mut every_family_reported = true;
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
        if entry.workload == "schema.index" {
            let stages = INDEX_STAGES.with(|held| held.borrow().clone());
            if !stages.is_empty() {
                println!("  {:<24} {stages}", "  last round");
            }
        }
        if entry.workload == "extension.fts.build" {
            let stages = FTS_STAGES.with(|held| held.borrow().clone());
            if !stages.is_empty() {
                println!("  {:<24} {stages}", "  last round");
            }
        }
    }

    println!();
    println!("## families");
    println!(
        "  {:<16} {:>9} {:>9} {:>9} {:>8} {:>9}  verdict",
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
                "  {family:<16} {:>9} {:>9} {:>9} {bar:>7.2}x  NO DATA",
                "-", "-", "-"
            );
            passed = false;
            every_family_reported = false;
            continue;
        }
        // The family is one log ratio per workload per round, weighted equally
        // per workload - `writegate`'s rollup, arrived at after two wrong ones.
        // Pooling raw pairs lets a workload with a hundred times
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

    // **The headline, weighted and unweighted, in that order and both of them.**
    // The weights are the checked-in ones in `compat/perf/contract.toml`, fixed
    // before any measurement was taken; the unweighted table is published beside
    // it so that a favourable mix cannot hide a slow primitive. The contract
    // also carries the floor, which is what a family is judged against here -
    // a family's *bar* is its TDD target and is reported above, and is a
    // different and higher thing.
    let contract = match Contract::parse(
        &std::fs::read_to_string(workspace_root().join("compat/perf/contract.toml"))
            .map_err(|error| format!("the performance contract is unreadable: {error}"))?,
    ) {
        Ok(contract) => contract,
        Err(reason) => return Err(format!("the performance contract does not parse: {reason}")),
    };
    let rounds = qualified_rounds(&measured);
    let full_plan = settings.families.len() == FAMILIES.len() && every_family_reported;
    println!();
    println!("## headline");
    if rounds.is_empty() {
        println!("  no round has a value for every workload, so there is no headline");
        passed = false;
    } else {
        let (centre, low, high) = weighted_headline(&rounds, &contract, SEED);
        let flat = Contract {
            families: contract
                .families
                .iter()
                .map(|family| inillucent_compat::perf::FamilyWeight {
                    weight: 1.0 / contract.families.len() as f64,
                    ..family.clone()
                })
                .collect(),
            ..contract.clone()
        };
        let (flat_centre, flat_low, flat_high) = weighted_headline(&rounds, &flat, SEED);
        println!(
            "  {:<26} {:>9} {:>9} {:>9} {:>8}  verdict",
            "geometric mean", "ratio", "low", "high", "bound"
        );
        // **Only a run of the whole plan is a headline.** A families-filtered
        // run is an iteration aid: its weights do not sum to one and the number
        // it would print is a different question wearing the answer's name.
        let met = low >= contract.headline && full_plan;
        println!(
            "  {:<26} {centre:>8.2}x {low:>8.2}x {high:>8.2}x {:>7.2}x  {}",
            "weighted, per the contract",
            contract.headline,
            if !every_family_reported {
                "A FAMILY REPORTED NOTHING - not a headline"
            } else if !full_plan {
                "PARTIAL RUN - not a headline"
            } else if met {
                "MET"
            } else {
                "MISSED"
            }
        );
        println!(
            "  {:<26} {flat_centre:>8.2}x {flat_low:>8.2}x {flat_high:>8.2}x {:>8}  reported",
            "unweighted, family-equal", "-"
        );
        println!(
            "  {} rounds contributed, each one weighted mean of that round's log ratios",
            rounds.len()
        );
        if full_plan {
            passed = passed && met;
        }
    }

    // The floor, which is a separate gate from every bar above: no required
    // family may sit below it however good the headline is.
    println!();
    println!("## floor: no required family below {:.2}x", contract.floor);
    let mut floored = true;
    for (family, _) in FAMILIES {
        if !settings.families.iter().any(|name| name == family) {
            continue;
        }
        if !contract.is_required(family) {
            continue;
        }
        let members: Vec<&Paired> = measured
            .iter()
            .filter(|entry| entry.family == family && entry.agreed && !entry.pairs.is_empty())
            .collect();
        if members.is_empty() {
            continue;
        }
        let (low, _) = pooled_interval(&members, SEED);
        if low < contract.floor {
            println!("  {family:<16} {low:>8.2}x  UNDER THE FLOOR");
            floored = false;
        }
    }
    if floored {
        println!("  every required family is above it");
    }
    passed = passed && floored;

    // **The two bars that are not about elapsed time.** They are judged only on
    // a full-plan run, for the same reason the headline is: the residency and
    // the processor time of a families-filtered round are a different quantity
    // wearing the same name, because the workloads that hold the memory may not
    // have run.
    passed = report_residency(&contract, child.as_ref(), &their_rounds, full_plan) && passed;

    // **The scratch goes with the run that made it.** Every round copies the
    // fixture twice and imports one of the copies, so a medium run leaves
    // roughly a hundred megabytes behind - and this binary left all of it, once
    // per run, under a pid-named directory nothing ever came back for.
    // 279 of them were found holding 60 GB in `%TEMP%`, and the
    // measurement itself pays for that: `txn.batched` is 200 commits and 200
    // syncs, and on a temp volume in that state it went from 47 ms to 240 ms on
    // *both* arms - the reference's own number moving with ours is what says it
    // is the disk rather than the engine.
    //
    // A **disagreement** leaves it, and only that: the two databases are the
    // evidence for a digest that did not match, and nothing else here is worth
    // a hundred megabytes. A missed bar is a number, not a file.
    if measured.iter().all(|entry| entry.agreed) {
        let _ = std::fs::remove_dir_all(&scratch);
        let _ = std::fs::remove_file(&plan_path);
    } else {
        println!();
        println!("  a workload disagreed; the two databases are kept in {scratch:?}");
    }

    println!();
    println!("## gate: {}", if passed { "MET" } else { "NOT MET" });
    Ok(passed)
}

/// Judges the peak resident set and the processor time against the contract.
///
/// **The child pair, and nothing else.** The engine's in-process figures are
/// deltas over a region of a process that also holds the harness, the plan and
/// thirty rounds of both arms; the reference's are the whole of a fresh child.
/// Only the pair of whole children - one each, one round, one budget - is a
/// comparison, so only it is judged.
///
/// A reading that could not be taken is reported as absent rather than as met:
/// `measure_in_a_child` returns `None` on a child that would not run, and a
/// gate that passed on a missing measurement is the failure this whole file
/// exists to prevent.
///
/// @param contract - the checked-in bars
/// @param child - what this engine's child cost, when one ran
/// @param theirs - the reference child's cost, one per round
/// @param full_plan - whether every family ran
fn report_residency(
    contract: &Contract,
    child: Option<&ChildRound>,
    theirs: &[ProcessCost],
    full_plan: bool,
) -> bool {
    use inillucent_compat::procstat::{mebibytes, millis};
    println!();
    println!("## memory and processor time, against the contract");
    if contract.memory.is_none() && contract.cpu.is_none() {
        println!("  the contract sets no memory or cpu bar");
        return true;
    }
    let Some(round) = child else {
        println!("  NOT MEASURED: this engine's child could not be run, so neither bar is judged");
        return false;
    };
    let mut peaks: Vec<f64> = theirs
        .iter()
        .map(|cost| mebibytes(cost.peak_working_set))
        .collect();
    let mut cpus: Vec<f64> = theirs.iter().map(|cost| millis(cost.cpu_nanos())).collect();
    let their_peak = middle(&mut peaks);
    let their_cpu = middle(&mut cpus);
    if their_peak <= 0.0 || their_cpu <= 0.0 {
        println!("  NOT MEASURED: the reference child reported no cost, so neither bar is judged");
        return false;
    }
    let our_peak = mebibytes(round.cost.peak_working_set);
    let our_cpu = millis(round.cost.cpu_nanos());
    println!(
        "  {:<28} {:>12} {:>12} {:>9} {:>8}  verdict",
        "quantity", "inillucent", "sqlite", "ratio", "bar"
    );
    let mut met = true;
    for (label, ours, reference, bar, unit) in [
        (
            "peak resident set",
            our_peak,
            their_peak,
            contract.memory,
            "MiB",
        ),
        ("processor time", our_cpu, their_cpu, contract.cpu, "ms"),
    ] {
        let ratio = ours / reference;
        let verdict = match (bar, full_plan) {
            (None, _) => "no bar".to_string(),
            (Some(_), false) => "PARTIAL RUN - not judged".to_string(),
            (Some(bar), true) => {
                if ratio <= bar {
                    "MET".to_string()
                } else {
                    met = false;
                    "MISSED".to_string()
                }
            }
        };
        println!(
            "  {label:<28} {:>9.2} {unit} {:>9.2} {unit} {ratio:>8.2}x {:>7}  {verdict}",
            ours,
            reference,
            bar.map(|bar| format!("{bar:.2}x"))
                .unwrap_or_else(|| "-".to_string()),
        );
    }
    println!("  one child process each, one round of the same plan, one matched memory budget");
    met
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
) -> Result<(Vec<Sample>, Vec<String>, RoundCost), String> {
    let copy = restore(fixture, scratch, "ours")?;
    let mut database = ImportedDatabase::import_with(copy, settings.page_size, settings.frames)
        .map_err(|error| format!("import failed: {}", why(&error)))?;
    round_on(&mut database, plan)
}

/// Runs one round of the plan against an open database.
///
/// Split out of `time_new_engine` so that the memory child can run exactly the
/// same round against a file it opened rather than imported - which is what
/// makes its peak comparable to the reference child's, since that one opens a
/// finished file too.
///
/// @param database - the engine to run against
/// @param plan - the workloads
fn round_on(
    database: &mut ImportedDatabase,
    plan: &inillucent_compat::perf::Plan,
) -> Result<(Vec<Sample>, Vec<String>, RoundCost), String> {
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
    let opened = ProcessCost::now();
    let mut costs: Vec<(String, ProcessCost, usize, LogCost)> =
        Vec::with_capacity(plan.workloads.len());
    let mut marks: Vec<(String, u64, u64, usize)> = Vec::with_capacity(plan.workloads.len());
    // **Two marks before the first workload**, because the two halves of the
    // fixed cost are different problems: what the process costs merely to exist
    // and open a file, and what filling the pool costs on top of it.
    let opening = ProcessCost::now();
    marks.push((
        "(open)".to_string(),
        opening.peak_working_set,
        opening.working_set,
        database.frames_resident(),
    ));
    for workload in &plan.workloads {
        // `pre` and `post` are setup, not work: `sqlite_bench.c` runs them
        // either side of the timed region and so does this.
        if let Some(pre) = &workload.pre {
            if let Err(reason) = run_batch(database, pre) {
                eprintln!("  {}: pre refused: {reason}", workload.name);
                continue;
            }
        }
        // Read either side of the timed region, exactly where the clock is.
        // The `pre` above and the `post` below are setup and are outside both.
        let before = ProcessCost::now();
        let log_before = database.wal().stats();
        let timed = if workload.mutates {
            time_write(database, workload, plan.rows)
        } else {
            time_read(database, workload, plan.rows)
        };
        let spent = ProcessCost::now().since(&before);
        let log_after = database.wal().stats();
        match timed {
            Ok(sample) => {
                costs.push((
                    workload.name.clone(),
                    spent,
                    database.frames_resident(),
                    LogCost {
                        writes: log_after.writes.saturating_sub(log_before.writes),
                        syncs: log_after.syncs.saturating_sub(log_before.syncs),
                        bytes: log_after.bytes.saturating_sub(log_before.bytes),
                    },
                ));
                samples.push(sample)
            }
            // A workload the engine refuses is *absent* rather than zero. A
            // sample of zero would roll into its family as an infinitely fast
            // one, which is the shape of lie a gate exists to prevent.
            Err(reason) => eprintln!("  {}: refused: {reason}", workload.name),
        }
        if let Some(post) = &workload.post {
            if let Err(reason) = run_batch(database, post) {
                eprintln!("  {}: post refused: {reason}", workload.name);
            }
        }
        // After `post`, so a workload that builds a fixture and drops it again
        // is charged for what it held while it held it.
        let standing = ProcessCost::now();
        marks.push((
            workload.name.clone(),
            standing.peak_working_set,
            standing.working_set,
            database.frames_resident(),
        ));
    }
    let round = ProcessCost::now().since(&opened);
    let mut state = Vec::with_capacity(AGREEMENT.len());
    for question in AGREEMENT {
        let answer = database
            .execute_any(question, &Params::new())
            .map_err(|error| format!("{question}: {}", why(&error)))?;
        state.push(render_row(&answer.rows));
    }
    Ok((
        samples,
        state,
        RoundCost {
            round,
            costs,
            marks,
        },
    ))
}

/// What one round of this engine's arm cost, besides time.
///
/// **Per workload for this arm and per round for the reference, because that
/// is what each one can honestly say.** This engine runs inside the gate, so a
/// reading either side of a workload's timed region is that workload's; the
/// reference is a child process running the whole plan, so the operating
/// system's accounting for it covers the round and nothing finer. Reporting
/// them as though they were the same measurement would be the more comfortable
/// lie.
struct RoundCost {
    /// What the round cost, from the first workload to the last.
    round: ProcessCost,
    /// Per workload: the name, what its timed region cost, how many pool frames
    /// were resident when it finished, and what it put in the log.
    costs: Vec<(String, ProcessCost, usize, LogCost)>,
    /// Per workload: the name, the process's **absolute** resident set and
    /// high-water mark once that workload had finished, and how many pool
    /// frames were resident at the same moment.
    ///
    /// The frames are here because the resident set on its own cannot say
    /// whether a rise was the buffer pool doing its job or the engine holding
    /// something it did not need to. Pool bytes are frames times the page size,
    /// and what is left over is everything else the process is holding.
    ///
    /// **A delta cannot say where a peak came from.** `PeakWorkingSetSize` only
    /// rises, so the workload that first reaches a number reports the whole
    /// rise and every later one reports zero - which reads as "only the first
    /// workload costs anything" rather than as "the mark has not moved since".
    /// The absolute pair does say it: the high-water mark's *steps* name the
    /// workloads that set the peak, and the resident set beside them says
    /// whether what they took was given back.
    marks: Vec<(String, u64, u64, usize)>,
}

/// What one workload asked of the write-ahead log.
///
/// **Because a write family's ratio is a durability ratio, not a code one.**
/// This engine measured three to four times slower on Linux than
/// on Windows on exactly the three workloads that `fsync` per commit, and
/// nowhere else - so "how many syncs, and how many bytes" is the question those
/// numbers raise, and nothing printed it.
#[derive(Clone, Copy, Default)]
struct LogCost {
    /// Calls to the log file's `write_all_at`.
    writes: u64,
    /// Calls to the log file's `sync`.
    syncs: u64,
    /// Bytes appended to the log.
    bytes: u64,
}

/// Prints what each arm cost besides time.
///
/// **Two tables rather than one, because the two arms can say different
/// things.** This engine runs inside the gate, so a reading either side of a
/// workload's timed region is that workload's; the reference is a child process
/// running the whole plan, so its accounting covers a round. Printing them in
/// one table under one heading would invite a comparison neither number
/// supports.
///
/// The medians are over rounds, so one round that happened to grow the heap
/// does not become the reported figure.
///
/// @param plan - the workloads, for their order
/// @param ours - what each of this engine's rounds cost
/// @param theirs - what each of the reference's rounds cost
fn report_costs(
    plan: &inillucent_compat::perf::Plan,
    ours: &[RoundCost],
    theirs: &[ProcessCost],
    child: Option<&ChildRound>,
    page_size: usize,
) {
    use inillucent_compat::procstat::{mebibytes, millis};
    if ours.is_empty() {
        return;
    }
    println!();
    println!("## memory and CPU, this engine, per workload   (median over rounds)");
    println!(
        "  {:<24} {:>12} {:>10} {:>10} {:>8} {:>8} {:>10}",
        "workload", "rss delta MiB", "cpu ms", "pool frames", "log wr", "log sync", "log KiB"
    );
    for workload in &plan.workloads {
        let mut rss: Vec<f64> = Vec::new();
        let mut cpu: Vec<f64> = Vec::new();
        let mut frames: Vec<f64> = Vec::new();
        let mut writes: Vec<f64> = Vec::new();
        let mut syncs: Vec<f64> = Vec::new();
        let mut bytes: Vec<f64> = Vec::new();
        for round in ours {
            let Some((_, cost, resident, log)) = round
                .costs
                .iter()
                .find(|(name, _, _, _)| *name == workload.name)
            else {
                continue;
            };
            rss.push(mebibytes(cost.working_set));
            cpu.push(millis(cost.cpu_nanos()));
            frames.push(*resident as f64);
            writes.push(log.writes as f64);
            syncs.push(log.syncs as f64);
            bytes.push(log.bytes as f64 / 1024.0);
        }
        if cpu.is_empty() {
            continue;
        }
        println!(
            "  {:<24} {:>12.2} {:>10.2} {:>10.0} {:>8.0} {:>8.0} {:>10.1}",
            workload.name,
            middle(&mut rss),
            middle(&mut cpu),
            middle(&mut frames),
            middle(&mut writes),
            middle(&mut syncs),
            middle(&mut bytes)
        );
    }

    println!();
    println!("## memory and CPU, per round, both arms   (median over rounds)");
    println!(
        "  {:<24} {:>12} {:>12} {:>10} {:>10}",
        "arm", "rss delta MiB", "peak rise MiB", "user ms", "kernel ms"
    );
    let mut our_rss: Vec<f64> = ours
        .iter()
        .map(|r| mebibytes(r.round.working_set))
        .collect();
    let mut our_peak: Vec<f64> = ours
        .iter()
        .map(|r| mebibytes(r.round.peak_working_set))
        .collect();
    let mut our_user: Vec<f64> = ours.iter().map(|r| millis(r.round.user_nanos)).collect();
    let mut our_kernel: Vec<f64> = ours.iter().map(|r| millis(r.round.kernel_nanos)).collect();
    println!(
        "  {:<24} {:>12.2} {:>12.2} {:>10.2} {:>10.2}",
        "inillucent (in process)",
        middle(&mut our_rss),
        middle(&mut our_peak),
        middle(&mut our_user),
        middle(&mut our_kernel)
    );
    // The reference's numbers are the *whole* child, not a delta: a fresh
    // process per round, so its peak is its peak and its time is all of it.
    let mut their_peak: Vec<f64> = theirs
        .iter()
        .map(|c| mebibytes(c.peak_working_set))
        .collect();
    let mut their_user: Vec<f64> = theirs.iter().map(|c| millis(c.user_nanos)).collect();
    let mut their_kernel: Vec<f64> = theirs.iter().map(|c| millis(c.kernel_nanos)).collect();
    println!(
        "  {:<24} {:>12} {:>12.2} {:>10.2} {:>10.2}",
        "sqlite (whole child)",
        "-",
        middle(&mut their_peak),
        middle(&mut their_user),
        middle(&mut their_kernel)
    );
    println!(
        "  process peak {:.2} MiB for the gate, which holds the harness and the plan too",
        mebibytes(ProcessCost::now().peak_working_set)
    );
    println!("  the engine's figures are deltas over a region of one process; the reference's");
    println!("  are the whole of a child that ran the whole plan. They are not one measurement.");
    println!();
    println!("## peak resident set, one child process each, one round of the same plan");
    match child.map(|round| (&round.cost, &round.marks)) {
        Some((cost, marks)) => {
            println!(
                "  {:<24} {:>12} {:>12} {:>10} {:>10}",
                "arm", "", "peak MiB", "user ms", "kernel ms"
            );
            println!(
                "  {:<24} {:>12} {:>12.2} {:>10.2} {:>10.2}",
                "inillucent (whole child)",
                "",
                mebibytes(cost.peak_working_set),
                millis(cost.user_nanos),
                millis(cost.kernel_nanos)
            );
            let mut their_peak: Vec<f64> = theirs
                .iter()
                .map(|c| mebibytes(c.peak_working_set))
                .collect();
            let mut their_user: Vec<f64> = theirs.iter().map(|c| millis(c.user_nanos)).collect();
            let mut their_kernel: Vec<f64> =
                theirs.iter().map(|c| millis(c.kernel_nanos)).collect();
            let peak = middle(&mut their_peak);
            println!(
                "  {:<24} {:>12} {:>12.2} {:>10.2} {:>10.2}",
                "sqlite (whole child)",
                "",
                peak,
                middle(&mut their_user),
                middle(&mut their_kernel)
            );
            if peak > 0.0 {
                println!(
                    "  inillucent holds {:.2}x what sqlite holds, running the same plan",
                    mebibytes(cost.peak_working_set) / peak
                );
            }
            println!("  both open a finished file the parent built; both run one round of the");
            println!("  plan; neither figure is a delta. This is the comparable pair.");
            report_marks(marks, page_size);
        }
        None => println!("  not measured: the child could not be run"),
    }
    // **The per-workload CPU column is at the clock's resolution and says so.**
    // `GetProcessTimes` advances in scheduler ticks - 15.625 ms on this
    // platform - so a workload that runs for five is reported as zero or as one
    // tick, and reading those as measurements would be reading the quantiser.
    // The per-round totals are hundreds of ticks and are the honest figures.
    println!("  per-workload CPU is quantised to the scheduler tick; read the per-round totals");
}

/// Runs one round of this engine in a child process, and returns what it cost.
///
/// **The comparable half of the memory question.** The gate's own process holds
/// the harness, the plan and both arms' thirty rounds, so its peak says nothing
/// about the engine; the reference arm, by contrast, is a fresh child per round
/// and its peak is exactly its own. Spawning this engine the same way puts the
/// two on one footing: a process that opens a finished file, runs the plan once,
/// and exits.
///
/// The import is done here, in the parent, for the same reason - the reference
/// child is handed a `.db` it did not have to build, so this child is handed an
/// `.rdb` it did not have to build either.
///
/// Returns `None` rather than failing the gate: a memory reading that could not
/// be taken must not turn a passing set of ratios into a failure.
///
/// @param fixture - the SQLite fixture to build the round's database from
/// @param scratch - where to put the copy
/// @param settings - the page size, frame count, scale and family filter
fn measure_in_a_child(fixture: &Path, scratch: &Path, settings: &Settings) -> Option<ChildRound> {
    let copy = restore(fixture, scratch, "ours-memory").ok()?;
    let target = scratch.join("ours-memory.rdb");
    let built =
        ImportedDatabase::import_into(copy, target.clone(), settings.page_size, settings.frames);
    if let Err(error) = built {
        eprintln!("  memory child: import failed: {}", why(&error));
        return None;
    }
    // Closed before the child opens it: two processes on one file is a thing
    // this engine does not do, and the parent holding it open would be that.
    drop(built);
    let exe = std::env::current_exe().ok()?;
    let mut child = Command::new(exe)
        .arg("--memory-round")
        .arg(&target)
        .arg("--scale")
        .arg(&settings.scale)
        .arg("--page-size")
        .arg(settings.page_size.to_string())
        .arg("--frames")
        .arg(settings.frames.to_string())
        .arg("--families")
        .arg(settings.families.join(","))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let mut err = String::new();
    let mut out = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut pipe, &mut out);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut pipe, &mut err);
    }
    let status = child.wait().ok()?;
    // Read while the handle is open, as `time_sqlite` does and for the same
    // reason: dropping the `Child` closes it and the accounting goes with it.
    let cost = inillucent_compat::procstat::child_cost(&child);
    if !status.success() {
        eprintln!("  memory child: {}", err.trim());
        return None;
    }
    Some(ChildRound {
        cost,
        marks: marks_from(&out),
    })
}

/// What the memory child reported: its own cost, and where its peak came from.
struct ChildRound {
    /// The operating system's accounting for the whole child.
    cost: ProcessCost,
    /// Per workload, in plan order: the name, the high-water mark once it had
    /// finished, and the resident set at the same moment, both in bytes.
    marks: Vec<(String, u64, u64, usize)>,
}

/// Reads the child's `mark` lines back.
///
/// @param out - everything the child wrote to its standard output
fn marks_from(out: &str) -> Vec<(String, u64, u64, usize)> {
    out.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            if parts.next()? != "mark" {
                return None;
            }
            let name = parts.next()?.to_string();
            let peak = parts.next()?.parse().ok()?;
            let resident = parts.next()?.parse().ok()?;
            let frames = parts.next()?.parse().ok()?;
            Some((name, peak, resident, frames))
        })
        .collect()
}

/// Returns the median of a list, sorting it in place.
///
/// @param values - the samples
fn middle(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let at = values.len() / 2;
    if values.len() % 2 == 1 {
        values.get(at).copied().unwrap_or(0.0)
    } else {
        let low = values.get(at.saturating_sub(1)).copied().unwrap_or(0.0);
        let high = values.get(at).copied().unwrap_or(0.0);
        (low + high) / 2.0
    }
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

thread_local! {
    /// Where the last `CREATE INDEX` this process ran spent its time.
    ///
    /// **The gate measures `schema.index` under conditions `indexprofile` does
    /// not reproduce**: the write workloads have run first, so the table has
    /// been inserted into, updated and deleted from before the index is built,
    /// and the compile is inside the clock. Reading the stage breakdown off a
    /// pristine import and assuming it holds here is how a family gets
    /// attributed to the wrong stage - it read 26.3 ms there and 38.4 ms here.
    /// The engine already times the stages; this carries the last round's out
    /// so the report can print them beside the ratio.
    static INDEX_STAGES: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };

    /// Where the last FTS5 index build this process ran spent its time.
    ///
    /// The same arrangement as `INDEX_STAGES`, and for the same reason: a
    /// workload under the floor needs its breakdown printed beside the number.
    /// `extension.fts.build` has been at 0.30x for two tickets and each of them
    /// had to re-derive where the time went; this prints it every run.
    static FTS_STAGES: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Renders where an FTS5 build spent its time, as one line.
///
/// Empty when nothing was indexed, so the caller prints nothing rather than a
/// row of zeroes.
///
/// @param stages - what the module measured
fn fts_stage_line(stages: inillucent_ext::vtab::fts5::BuildStages) -> String {
    if stages.rows == 0 {
        return String::new();
    }
    let ms = |nanos: u128| nanos as f64 / 1e6;
    format!(
        "{} rows, content {:.1} ms, tokenize {:.1} ms, docsize {:.1} ms, \
         group {:.1} ms, terms {:.1} ms, new terms {:.1} ms ({}), \
         dict read {:.1} ms, dict write {:.1} ms, totals {:.1} ms, flush {:.1} ms",
        stages.rows,
        ms(stages.content),
        ms(stages.tokenize),
        ms(stages.docsize),
        ms(stages.group),
        ms(stages.terms.saturating_sub(stages.new_terms)),
        ms(stages.new_terms),
        stages.new_term_count,
        ms(stages.dictionary_read),
        ms(stages.dictionary_write),
        ms(stages.totals),
        ms(stages.flush),
    )
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
        if workload.family == "schema" {
            let stages = database.build_stages();
            INDEX_STAGES.with(|held| *held.borrow_mut() = stages);
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
    // Cleared before the clock rather than after it, so the line printed is the
    // last round's build and not every round's added together.
    if workload.name == "extension.fts.build" {
        inillucent_ext::vtab::fts5::reset_build_stages();
    }
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
    let nanos = started.elapsed().as_secs_f64() * 1e9;
    if workload.name == "extension.fts.build" {
        let line = fts_stage_line(inillucent_ext::vtab::fts5::build_stages());
        FTS_STAGES.with(|held| *held.borrow_mut() = line);
    }
    Ok(Sample {
        workload: workload.name.clone(),
        nanos,
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
) -> Result<
    (
        Vec<Sample>,
        Vec<String>,
        inillucent_compat::procstat::ProcessCost,
    ),
    String,
> {
    let copy = restore(fixture, scratch, "theirs")?;
    // **Spawned rather than `output()`ed, so the process can be asked what it
    // cost.** `output()` reaps the child, and a reaped process has no handle
    // left to read its peak resident set or its processor time from. The pipes
    // are drained before the wait for the ordinary reason: a child that fills
    // one blocks, and a parent that waits first would deadlock with it.
    let mut child = Command::new(bench)
        .arg("run")
        .arg(plan)
        .arg(&copy)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("sqlite-bench did not start: {error}"))?;
    let mut out = String::new();
    let mut err = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut pipe, &mut out);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut pipe, &mut err);
    }
    let status = child
        .wait()
        .map_err(|error| format!("sqlite-bench did not finish: {error}"))?;
    // Read while the handle is still open, which is the whole reason for the
    // spawn: dropping the `Child` closes it and the accounting goes with it.
    let cost = inillucent_compat::procstat::child_cost(&child);
    if !status.success() {
        return Err(format!("sqlite-bench failed: {err}"));
    }
    let samples: Vec<Sample> = out.lines().filter_map(Sample::parse).collect();
    let mut state = Vec::with_capacity(AGREEMENT.len());
    for question in AGREEMENT {
        state.push(ask_sqlite(&copy, question)?);
    }
    Ok((samples, state, cost))
}

/// Asks SQLite one question about the database it just wrote.
///
/// @param database - the copy SQLite wrote to
/// @param sql - the question
fn ask_sqlite(database: &Path, sql: &str) -> Result<String, String> {
    let shell = workspace_root().join(format!(
        ".sqlite-ref/3.53.4/shell/sqlite3{}",
        std::env::consts::EXE_SUFFIX
    ));
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
    // **This platform's binary, not whichever name happens to exist.** Both are
    // checked in beside each other, and the list used to try `.exe` first on
    // every platform. Under WSL that is not a missing file, it is a *running*
    // one: binfmt happily executes the Windows build, which is then handed a
    // Linux plan path it cannot resolve and reports
    // `cannot open /tmp/inillucent-fullgate-<pid>.plan` - a file that is plainly
    // there. The Linux arm of the portability gate failed on that and nothing
    // else.
    for name in [
        format!("sqlite-bench{}", std::env::consts::EXE_SUFFIX),
        "sqlite-bench".to_string(),
    ] {
        let path = root.join(&name);
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

/// Prints where the memory child's high-water mark actually came from.
///
/// **The attribution the ticket kept having to take by hand.** Review 5 found
/// where the memory was by running the gate once per family and reading one
/// number off each run - seven runs to answer one question, and a number per
/// family rather than per workload. The child already walks the whole plan in
/// one process; reading its mark after every workload gives the same
/// attribution in a single run, at the granularity the optimisation work
/// actually needs.
///
/// Only the workloads that *moved* the mark are printed. A workload that did
/// not raise it took less than something before it, which is a fact about the
/// earlier workload, and printing thirty rows of "no change" would bury the
/// four that matter.
///
/// @param marks - the child's per-workload high-water mark and resident set
fn report_marks(marks: &[(String, u64, u64, usize)], page_size: usize) {
    use inillucent_compat::procstat::mebibytes;
    if marks.is_empty() {
        return;
    }
    println!();
    println!("## what raised the child's high-water mark, workload by workload");
    println!(
        "  {:<24} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "workload", "peak MiB", "rise MiB", "rss MiB", "pool MiB", "other MiB"
    );
    let mut previous = 0_u64;
    for (name, peak, resident, frames) in marks {
        let rise = peak.saturating_sub(previous);
        let pool = frames.saturating_mul(page_size) as u64;
        if rise > 0 {
            println!(
                "  {name:<24} {:>10.2} {:>10.2} {:>10.2} {:>10.2} {:>10.2}",
                mebibytes(*peak),
                mebibytes(rise),
                mebibytes(*resident),
                mebibytes(pool),
                mebibytes(resident.saturating_sub(pool))
            );
        }
        previous = (*peak).max(previous);
    }
    println!("  every other workload left the mark where it already was");
    println!("  pool = frames resident times the page size; other = everything else the");
    println!("  process holds, which is the number the optimisation work is about");
}
