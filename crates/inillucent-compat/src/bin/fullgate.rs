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
//!   inillucent-fullgate `<sqlite fixture>` [--rounds N] [--page-size N]
//!                       [--scale S] [--frames N] [--families a,b] [--repeat N]
//!                       [--module-split] [--put-split]
//!
//! `--put-split` prints, under every workload that writes rows, where one row's
//! write into a leaf went: encoding the row, finding the leaf, locating the key
//! in it, the room check, the undo record, and then `apply_row` split into the
//! extent question, the log record and the page write. It answers the question
//! `--module-split` left: that split ended at `PagedTree::put` costing 1.8 to
//! 2.2 us for a write that descends nothing and compacts nothing, and said
//! nothing about what is inside it. It is off for the same reason
//! `--module-split` is, and it applies to every family rather than to
//! `extension` alone, because `write_row` is the path an ordinary `INSERT`
//! reaches too and whether the cost is the write path's or virtual tables' is
//! the question it exists to answer.
//!
//! `--module-split` prints, under each `extension` workload that writes to a
//! module, where the write went: the module's own `update`, the arm above it,
//! the commit, every shadow row write split into borrowing the row and writing
//! the tree, and whether those writes descended or reused the leaf the hint
//! names. It is off by default because the timing costs enough to move the
//! ratio it sits beside - `extension.rtree.insert` went from 1.29x to 1.10x
//! with it always on - so a run that decides a bar does not carry it.

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

use inillucent_compat::affinity::{self, Placement};
use inillucent_compat::newengine::connect::{
    Connection as ConnectedConnection, Database as ConnectedDatabase,
    Statement as ConnectedStatement,
};
use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::perf::{bind_value, eat_borrowed};
use inillucent_compat::perf::{
    plan_for, qualified_rounds, weighted_headline, Contract, Digest, Grouping, Paired, Sample,
    Workload,
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
///
/// Each bar is a bar on the family's geometric mean over its workloads, which
/// is what `perf::family_interval` grades. `read.join`'s 3.00x missed on every
/// build until task-2093, because the statistic that graded it measured the
/// distance between `join.selective` and `join.range`. task-2093 kept it at
/// 3.00x: it is the TDD's number for the statistic the TDD meant, and every
/// build since task-1819 meets it under that statistic, `b0ba286` by 6% on this
/// gate's plan. `docs/performance.md` has the five builds.
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
    /// Whether to time and print where a virtual table write's time goes.
    module_split: bool,
    /// Whether to time and print where one row's write into a leaf goes.
    put_split: bool,
    /// Which way into this engine the arm drives (task-2066 section 4.3.10).
    api: Api,
    /// Where every round's raw timings are appended, when asked (task-2095).
    samples: Option<PathBuf>,
    /// Whether this engine's arm runs each round in a fresh child process, the
    /// way the reference arm always has (task-2095).
    engine_child: bool,
    /// Whether a busy machine stops the pass being graded, and whether to record a reference (task-2110).
    quiet: inillucent_compat::quiet::Options,
}

fn main() -> ExitCode {
    let mut arguments: Vec<String> = std::env::args().skip(1).collect();
    // **Taken off the command line before anything else reads it (task-2085)**,
    // so the positional fixture and every `flag` lookup see what they always saw.
    let cores = match affinity::take_cores_flag(&mut arguments) {
        Ok(cores) => cores,
        Err(reason) => {
            eprintln!("full gate: {reason}");
            return ExitCode::from(2);
        }
    };
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
    // **A child that times one round of this engine (task-2095)**, spawned per
    // round by `time_in_a_fresh_child` when the gate is given `--engine-child`.
    if let Some(database) = flag(&arguments, "--engine-round") {
        return match engine_round(Path::new(&database), &settings_from(&arguments)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(reason) => {
                eprintln!("engine round: {reason}");
                ExitCode::from(2)
            }
        };
    }
    let Some(fixture) = arguments.first().filter(|first| !first.starts_with("--")) else {
        eprintln!(
            "usage: inillucent-fullgate <sqlite fixture> [--rounds N] [--page-size N] \
             [--scale S] [--frames N] [--families a,b] [--repeat N] [--locking normal|exclusive] \
             [--module-split] [--put-split] [--cores performance|efficiency|any] \
             [--samples <file>] [--engine-child] [--quiet-threshold PERCENT]              [--record-quiet-reference]"
        );
        return ExitCode::from(2);
    };
    let settings = settings_from(&arguments);
    // **Pinned before the fixture is read and before any child exists
    // (task-2085).** With no affinity set, Windows ran this process on the
    // efficiency cores and `sqlite-bench` on the performance cores, so every
    // paired round compared two kinds of hardware. A child inherits the mask,
    // and `time_sqlite` checks that it did.
    let placement = match affinity::pin(cores) {
        Ok(placement) => placement,
        Err(reason) => {
            eprintln!(
                "full gate: could not pin to the {} cores: {reason}",
                cores.name()
            );
            return ExitCode::from(2);
        }
    };
    match run(Path::new(fixture), &settings, &placement) {
        Ok(Some(true)) => ExitCode::SUCCESS,
        Ok(Some(false)) => ExitCode::from(1),
        // Measured and not graded, because the machine was not quiet (task-2110).
        Ok(None) => ExitCode::from(inillucent_compat::quiet::NOT_GRADED),
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
        // **`pipeline` by default, so no published number moves.** Every
        // figure in `docs/performance.md` was taken through `plan`, `prepare`
        // and `pipeline`, and changing what this binary measures by default
        // would silently restate all of them. `--api connection` measures the
        // shipped API instead and `--api both` measures the two in one round,
        // which is the only way to compare them on a machine that moves.
        api: match flag(arguments, "--api").unwrap_or_default().as_str() {
            "connection" => Api::Connection,
            "both" => Api::Both,
            _ => Api::Pipeline,
        },
        // **Off by default, because the split is not free (task-2025).** It
        // times every shadow row write and every pass through the insert arm,
        // and measured always-on it took `extension.rtree.insert` from 1.29x to
        // 1.10x. A gate run that decides whether a bar is met must be the code
        // an application runs, so the breakdown is asked for by name.
        module_split: arguments.iter().any(|value| value == "--module-split"),
        // **Off for the same reason, and measured rather than assumed
        // (task-2034).** Fifteen clock reads a row against a write that costs
        // two microseconds is the same arithmetic `--module-split` failed, so
        // the split is asked for by name and a run that decides a bar does not
        // ask. Measured on the same binary, three runs each: with it on,
        // `extension.fts.build` reads 0.37x, 0.41x, 0.42x and
        // `extension.rtree.insert` 1.04x, 1.01x, 1.08x; with it off they read
        // 0.46x, 0.45x, 0.42x and 1.37x, 1.14x, 1.28x.
        put_split: arguments.iter().any(|value| value == "--put-split"),
        // **Both off by default, so no published number moves (task-2095).**
        // `--samples` only writes a file. `--engine-child` changes what this
        // arm pays inside its clock - process start, first touches of fresh
        // memory - to what the reference arm has always paid, and a number
        // taken that way is a different number from every published one.
        samples: flag(arguments, "--samples").map(PathBuf::from),
        engine_child: arguments.iter().any(|value| value == "--engine-child"),
        quiet: inillucent_compat::quiet::Options::from_arguments(arguments),
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
    let (_, _, cost) = round_on(&mut opened, &plan, splits_of(settings))?;
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

/// Times one round of the plan in this child process and prints the samples.
///
/// **The same round the in-process arm runs, in a process that starts fresh
/// (task-2095).** The parent built the file, so the import is outside every
/// clock, and `round_on` warms the pool before the first workload exactly as it
/// does in process. What changes is only what the reference child has always
/// paid inside its clock: memory this process touches for the first time.
///
/// Every line carries a tag, `sample` or `state`, because `round_on` and the
/// splits print lines of their own.
///
/// @param database - the `.rdb` the parent built
/// @param settings - the page size, frame count, scale, families and lock mode
fn engine_round(database: &Path, settings: &Settings) -> Result<(), String> {
    let mut plan = filtered_plan(settings)?;
    plan.locking.clone_from(&settings.locking);
    let mut opened =
        ImportedDatabase::open(database.to_path_buf(), settings.page_size, settings.frames)
            .map_err(|error| format!("open failed: {}", why(&error)))?;
    let (samples, state, _) = round_on(&mut opened, &plan, splits_of(settings))?;
    for sample in &samples {
        println!("sample\t{}", sample.render());
    }
    for answer in &state {
        println!("state\t{answer}");
    }
    Ok(())
}

/// Times one round of this engine in a fresh child process.
///
/// The import happens here, in the parent and outside every clock, into one
/// reused name, which `import_into` removes before it builds - the same thing
/// the in-process arm does through `import_with`. The file is closed before the
/// child opens it, because two processes on one file is a thing this engine
/// does not do.
///
/// @param fixture - the pristine SQLite database
/// @param scratch - where the copy and the built file go
/// @param settings - the page size, frame count, scale, families and lock mode
fn time_in_a_fresh_child(
    fixture: &Path,
    scratch: &Path,
    settings: &Settings,
) -> Result<(Vec<Sample>, Vec<String>, RoundCost), String> {
    let copy = restore(fixture, scratch, "ours-child")?;
    let target = scratch.join("ours-child.rdb");
    let built =
        ImportedDatabase::import_into(copy, target.clone(), settings.page_size, settings.frames)
            .map_err(|error| format!("import failed: {}", why(&error)))?;
    drop(built);
    let exe = std::env::current_exe().map_err(|error| format!("no executable: {error}"))?;
    let mut child = affinity::spawn_on_same_cores(
        Command::new(exe)
            .arg("--engine-round")
            .arg(&target)
            .args(child_arguments(settings))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()),
        "the engine child",
    )?;
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
        .map_err(|error| format!("the engine child did not finish: {error}"))?;
    let cost = inillucent_compat::procstat::child_cost(&child);
    if !status.success() {
        return Err(format!("the engine child failed: {}", err.trim()));
    }
    let samples = out
        .lines()
        .filter_map(|line| line.strip_prefix("sample\t"))
        .filter_map(Sample::parse)
        .collect();
    let state = out
        .lines()
        .filter_map(|line| line.strip_prefix("state\t"))
        .map(str::to_string)
        .collect();
    let round = RoundCost {
        round: cost,
        costs: Vec::new(),
        marks: Vec::new(),
    };
    Ok((samples, state, round))
}

/// Returns the flags a child needs to run the same plan the parent runs.
///
/// @param settings - what the parent was asked to measure
fn child_arguments(settings: &Settings) -> Vec<String> {
    let mut arguments = vec![
        "--scale".to_string(),
        settings.scale.clone(),
        "--page-size".to_string(),
        settings.page_size.to_string(),
        "--frames".to_string(),
        settings.frames.to_string(),
        "--families".to_string(),
        settings.families.join(","),
        "--locking".to_string(),
        settings.locking.clone(),
    ];
    if let Some(repeat) = settings.repeat_override {
        arguments.push("--repeat".to_string());
        arguments.push(repeat.to_string());
    }
    arguments
}

/// Appends one round's raw timings, both arms, to the samples file.
///
/// **What the family table cannot give back (task-2095).** The report prints a
/// median per workload and an interval per family, and neither separates what
/// moved between rounds from what moved between passes, or says which arm
/// moved. One line per arm per workload per round does, and so do the two
/// processes' costs for the round. A write that fails is reported and does not
/// stop the gate: the file is evidence about the run, not part of its verdict.
///
/// @param path - the file to append to
/// @param round - the round's index
/// @param elapsed - seconds since the first round started
/// This engine's page faults are also written per workload, because its arm
/// runs in this process and can be read either side of each timed region; the
/// reference's can only be read for its whole child. A round run in a fresh
/// child has no per workload costs, and writes none.
///
/// @param path - the file to append to
/// @param round - the round's index
/// @param elapsed - seconds since the first round started
/// @param ours - this engine's samples and what its round cost
/// @param theirs - the reference's samples and what its child cost
fn record_samples(
    path: &Path,
    round: u32,
    elapsed: f64,
    ours: (&[Sample], &RoundCost),
    theirs: (&[Sample], &ProcessCost),
) {
    let mut text = String::new();
    for (workload, cost, _, _) in &ours.1.costs {
        text.push_str(&format!(
            "{round}\t{elapsed:.3}\tours\t(faults)\t{workload}\t{}\n",
            cost.page_faults
        ));
    }
    for (arm, samples, cost) in [
        ("ours", ours.0, &ours.1.round),
        ("theirs", theirs.0, theirs.1),
    ] {
        for sample in samples {
            text.push_str(&format!(
                "{round}\t{elapsed:.3}\t{arm}\t{}\t{:.0}\n",
                sample.workload, sample.nanos
            ));
        }
        text.push_str(&format!(
            "{round}\t{elapsed:.3}\t{arm}\t(cost)\tuser {} kernel {} faults {} peak {}\n",
            cost.user_nanos, cost.kernel_nanos, cost.page_faults, cost.peak_working_set
        ));
    }
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, text.as_bytes()));
    if let Err(error) = written {
        eprintln!("  samples: could not append to {path:?}: {error}");
    }
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
    // **A workload whose family no table weights still runs** (task-2066
    // §4.3.1). `--families` names which of the ten weighted families to
    // measure and its default is all of them, so a filter by membership drops
    // a workload that is deliberately outside the weighting - which is what
    // `read.correlated` is, and `perf::correlated_read_workloads` gives the
    // reason. It is reported per workload and reaches no family, no floor and
    // no headline. An explicit `--families` still selects, because a name the
    // caller did not ask for is a name they did not ask for.
    let asked = settings.families.clone();
    plan.workloads.retain(|workload| {
        asked.contains(&workload.family)
            || !FAMILIES.iter().any(|(name, _)| *name == workload.family)
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
/// Prints the configuration block that opens the report.
///
/// **Lifted out of [`run`] because a banner is not a measurement
/// (task-1969, 7.2).** `run` was 389 lines and the ratchet in `policy.rs`
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
    // **First, because it decides whether any number below means anything
    // (task-2085).** An unpinned run on a hybrid processor can put the two
    // arms on different core classes, and nothing else in this block would
    // show it.
    placement.print_configuration();
    if placement.pinned {
        // task-2064 measured `correlated.exists` at 59.69 ms on the 8
        // performance cores, 46.35 ms on the 16 efficiency cores and 38.74 ms
        // unpinned on all 24. It is the one workload that gets slower when
        // pinned, because it uses more processors than the mask allows.
        println!(
            "                read.correlated uses more than one thread and reads slower pinned:"
        );
        println!("                correlated.exists was 59.69 ms on 8 performance cores against");
        println!(
            "                38.74 ms unpinned on all 24 (task-2064); --cores any for that figure"
        );
    }
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
    println!("  sqlite lock : locking_mode = {}", settings.locking);
    // **It used to say "this engine takes no file lock at all", and that stopped
    // being true two releases ago** (task-2000, design 1d). `locking_mode = normal`
    // is the shipped default and under it this engine takes SHARED, RESERVED and
    // EXCLUSIVE on every statement and lets them go again - which is the whole
    // reason the `write` and `transaction` families cost what they do. A gate that
    // told a reader the two arms were locking differently when they were not is a
    // gate that was describing an older engine.
    println!("  inillucent lock: locking_mode = normal, the shipped default - the file is");
    println!("                taken and released once per statement, exactly as SQLite's arm does");
    println!("  api         : {}", settings.api.name());
    // task-2095: which process this engine's arm runs in is part of what it
    // measures, so a run that changed it says so before anything is timed.
    println!(
        "  engine arm  : {}",
        if settings.engine_child {
            "a fresh child process per round, as the reference arm is"
        } else {
            "inside this process, every round"
        }
    );
    if settings.api.drives_a_connection() {
        println!(
            "                the connection arm goes through Connection::prepare and Statement::step,"
        );
        println!(
            "                which is the only route an application outside this workspace has"
        );
    }
    // **True of the pipeline arm and false of the connection arm**, so the
    // line is printed per arm rather than as a fact about the binary. A
    // `Connection::prepare` looks the statement up in the plan cache, which is
    // most of the difference section 4.3.10 exists to measure.
    if settings.api.drives_the_pipeline() {
        println!("  plan cache  : declared, and NOT used by the pipeline arm of this gate");
    }
    if settings.api.drives_a_connection() {
        println!("  plan cache  : USED by the connection arm - Connection::prepare looks a");
        println!("                statement up by text, which is part of what that arm costs");
    }
    if settings.api.drives_the_pipeline() {
        println!(
            "                inillucent keeps a prepared plan per statement text, and the TDD names"
        );
        println!(
            "                it as the thing a reader is most likely to contest. The pipeline arm"
        );
        println!("                does not reach it: a prepare-each workload calls plan() and");
        println!(
            "                prepare() inside the clock, and plan() parses, binds and plans on"
        );
        println!(
            "                every call; every other workload prepares once, outside the clock,"
        );
        println!("                and rebinds. So no pipeline number is helped by the cache, and");
        println!("                SQLite compiles per iteration for a prepare-each workload too.");
    }
    println!(
        "  warm state  : inillucent's pool is filled before each round; SQLite's cache fills as the plan runs"
    );
    println!("  durability  : synchronous = FULL on both arms");
    println!();
}

/// Prints the per-family verdict table, and returns whether every family met
/// its bar and whether every one of them reported at all.
///
/// **Lifted out of [`run`] (task-1969, 7.2).** `run` was 354 lines, and this
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
fn report_families(settings: &Settings, measured: &[Paired], graded: bool) -> (bool, bool) {
    let mut met_every_family = true;
    // Whether every family the contract weights actually reported. A headline
    // weighted over a plan that skipped one is a headline about a different
    // plan, and `run` refuses to publish one.
    let mut every_family_reported = true;
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
            met_every_family = false;
            every_family_reported = false;
            continue;
        }
        // The family is one value per round, the mean of that round's log
        // ratios over the family's workloads, and the bootstrap resamples
        // rounds - the statistic the headline already uses for each family.
        // Pooling raw pairs lets a workload with a hundred times the absolute
        // time decide the family alone; collapsing each workload to its median
        // first makes a three-point bootstrap whose lower bound *is* the
        // minimum; and one list of every workload's every round, which this
        // used until task-2086, made the interval measure the gap between the
        // workloads. See `perf::family_interval`.
        let (low, high) = family_bounds(&members, SEED);
        let worst = members
            .iter()
            .map(|entry| entry.ratio())
            .fold(f64::INFINITY, f64::min);
        // **The lower bound against the bar, not the point estimate.**
        let met = low >= bar;
        met_every_family = met_every_family && met;
        println!(
            "  {family:<16} {:>8.2}x {:>8.2}x {:>8.2}x {bar:>7.2}x {:>8.2}x  {}",
            geometric_mean(&members),
            low,
            high,
            worst,
            inillucent_compat::quiet::verdict(graded, met)
        );
    }
    (met_every_family, every_family_reported)
}

/// Prints the per-workload result table, and returns whether every workload
/// agreed with SQLite and produced a sample.
///
/// **Lifted out of [`run`] (task-1969, 7.2).** `run` was 309 lines; this is one
/// stage of it, and the only thing it decides is the one value it returns.
///
/// **A family that produced no sample must not be renormalised away.**
/// `weighted_mean` averages over the families a round actually has and divides
/// by the weight it used, so a workload that fails outright can make the
/// headline go *up*: at large, `schema.index` could not run at all and the
/// headline read 4.16x; with the same runs and `schema` present at 0.58x it
/// reads 3.86x. A number that improves when a family breaks is not a headline,
/// and this is what stops it being printed as one.
///
/// @param measured - every workload's paired rounds
/// @returns whether every workload agreed and produced a sample
fn report_results(measured: &[Paired]) -> bool {
    let mut every_workload_agreed = true;
    println!();
    println!("## result");
    println!(
        "  {:<24} {:>14} {:>14} {:>9} {:>9} {:>9}  agreed",
        "workload", "inillucent ns", "sqlite ns", "ratio", "low", "high"
    );
    // **A family that produced no sample must not be renormalised away.**
    // `weighted_mean` averages over the families a round actually has and
    // divides by the weight it used, so a workload that fails outright makes
    // the headline go *up*: at large, `schema.index` could not run at all and
    // the headline read 4.16x; with the same runs and `schema` present at
    // 0.58x it reads 3.86x. A number that improves when a family breaks is not
    // a headline, and this is what stops it being printed as one.
    for entry in measured {
        if !entry.agreed || entry.pairs.is_empty() {
            println!(
                "  {:<24} {:>14} {:>14} {:>9} {:>9} {:>9}  NO: {}",
                entry.workload, "-", "-", "-", "-", "-", entry.disagreement
            );
            every_workload_agreed = false;
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
        print_stage_lines(&entry.workload);
    }
    every_workload_agreed
}

/// @param placement - the processors this process was pinned to
fn run(fixture: &Path, settings: &Settings, placement: &Placement) -> Result<Option<bool>, String> {
    let bench = sqlite_bench().ok_or_else(|| {
        "sqlite-bench is not built; run tools/sqlite-reference.ps1 first".to_string()
    })?;

    let mut plan = filtered_plan(settings)?;
    plan.locking.clone_from(&settings.locking);
    let pool_bytes = settings.frames.saturating_mul(settings.page_size);
    plan.cache_size = -((pool_bytes / 1024) as i32);

    print_configuration(settings, &plan, pool_bytes, placement);
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
    let mut api_pairs = api_slots(settings, &plan);
    let arm_inputs = ArmInputs::new(fixture, &scratch, &plan, settings);
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
        if let Some(path) = &settings.samples {
            let elapsed = started.elapsed().as_secs_f64();
            record_samples(
                path,
                round,
                elapsed,
                (&ours, &our_cost),
                (&theirs, &their_cost),
            );
        }
        our_rounds.push(our_cost);
        their_rounds.push(their_cost);
        run_the_connection_arm(&mut api_pairs, &arm_inputs, round, &ours, &our_state)?;
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
        pair_against_the_reference(&mut measured, &plan, &ours, &theirs)?;
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

    // Before any verdict is printed: see `inillucent_compat::quiet` (task-2110).
    let graded = inillucent_compat::quiet::check(&measured, &plan, &settings.quiet).graded();

    let mut passed = report_results(&measured);

    // Reported, and it does not decide the gate: the two arms being apart is a
    // cost to explain on the performance page, not a regression against SQLite.
    report_the_two_ways_in(&api_pairs);

    let (met_every_family, every_family_reported) = report_families(settings, &measured, graded);
    passed = passed && met_every_family;

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
            } else {
                inillucent_compat::quiet::verdict(graded, met)
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
        let (low, _) = family_bounds(&members, SEED);
        if low < contract.floor {
            println!(
                "  {family:<16} {low:>8.2}x  {}",
                if graded {
                    "UNDER THE FLOOR"
                } else {
                    "under the floor, NOT GRADED"
                }
            );
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
    passed =
        report_residency(&contract, child.as_ref(), &their_rounds, full_plan, graded) && passed;

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
    println!(
        "## gate: {}",
        inillucent_compat::quiet::gate_line(graded, passed)
    );
    Ok(graded.then_some(passed))
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
/// @param graded - false when the machine was not quiet, so neither bar is MET or MISSED
fn report_residency(
    contract: &Contract,
    child: Option<&ChildRound>,
    theirs: &[ProcessCost],
    full_plan: bool,
    graded: bool,
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
                met = met && ratio <= bar;
                inillucent_compat::quiet::verdict(graded, ratio <= bar).to_string()
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
    // **`--api connection` replaces this arm rather than adding to it**, so a
    // run asking only for the shipped API compares that against SQLite. With
    // `--api both` this stays the pipeline and the connection arm runs beside
    // it in the same round; see `run`.
    if settings.api == Api::Connection {
        return time_through_a_connection(fixture, scratch, plan, settings);
    }
    if settings.engine_child {
        return time_in_a_fresh_child(fixture, scratch, settings);
    }
    let copy = restore(fixture, scratch, "ours")?;
    let mut database = ImportedDatabase::import_with(copy, settings.page_size, settings.frames)
        .map_err(|error| format!("import failed: {}", why(&error)))?;
    round_on(&mut database, plan, splits_of(settings))
}

/// Which of the two ways into this engine an arm drives.
///
/// **They are different amounts of code and nothing measured the second**
/// (task-2066 §4.3.10). `inillucent-fullgate` has always driven `plan`,
/// `prepare` and `pipeline` directly, which is the shortest path to an answer
/// and not the one an application has. A `Connection` adds the plan cache
/// lookup, the parameter count, the per-execution column names and the dirty
/// frame walk on release - and `docs/performance.md:529` already said this
/// arm was missing. Every figure on that page is the pipeline's.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Api {
    /// `plan`, `prepare` and `pipeline`, which is what every published number is.
    Pipeline,
    /// `Connection::prepare` and `Statement::step`, which is what a caller has.
    Connection,
    /// Both, in the same round, so the difference is paired rather than compared
    /// across two runs of the binary on a machine that moved in between.
    Both,
}

impl Api {
    /// Whether the pipeline-driven arm runs.
    fn drives_the_pipeline(self) -> bool {
        matches!(self, Api::Pipeline | Api::Both)
    }

    /// Whether the `Connection`-driven arm runs.
    fn drives_a_connection(self) -> bool {
        matches!(self, Api::Connection | Api::Both)
    }

    /// The name this arm reports under.
    fn name(self) -> &'static str {
        match self {
            Api::Pipeline => "pipeline",
            Api::Connection => "connection",
            Api::Both => "both",
        }
    }
}

/// Times every workload through the shipped `Connection` API.
///
/// **The same fixture, the same plan, the same digest, a different entry
/// point.** The pipeline arm beside it calls `plan`, `prepare` and `pipeline`;
/// this one calls `Connection::prepare` and steps the `Statement`, which is
/// the only thing an application outside this workspace can do. What sits
/// between the two is the plan cache lookup, `parameter_count`'s second parse,
/// a `String` per result column per execution, and `dirty_pages()`'s walk of
/// every frame on the release path - sections 4.3.2, 4.3.3 and 4.3.5, none of
/// which any published figure can see.
///
/// The fixture is imported by the same code the pipeline arm imports with, and
/// then *opened* through the shipped API, so the bytes under the two arms are
/// the same bytes.
///
/// @param fixture - the pristine SQLite database
/// @param scratch - where the copy goes
/// @param plan - the plan, for its workloads and row count
/// @param settings - the page size and pool size
fn time_through_a_connection(
    fixture: &Path,
    scratch: &Path,
    plan: &inillucent_compat::perf::Plan,
    settings: &Settings,
) -> Result<(Vec<Sample>, Vec<String>, RoundCost), String> {
    let copy = restore(fixture, scratch, "ours-connection")?;
    let mut built = copy.clone().into_os_string();
    built.push(".rdb");
    let built = PathBuf::from(built);
    // Imported and then dropped, so what the arm opens is a file on disk that
    // the shipped `open_at` read - not a handle the import left behind.
    drop(
        ImportedDatabase::import_into(copy, built.clone(), settings.page_size, settings.frames)
            .map_err(|error| format!("import failed: {}", why(&error)))?,
    );
    let database = ConnectedDatabase::open_at(&built, settings.page_size, settings.frames)
        .map_err(|error| format!("open failed: {}", why(&error)))?;
    round_through_a_connection(&database, plan)
}

/// Runs one round of the plan over an open `Connection`.
///
/// @param database - the open database
/// @param plan - the plan, for its workloads and row count
fn round_through_a_connection(
    database: &ConnectedDatabase,
    plan: &inillucent_compat::perf::Plan,
) -> Result<(Vec<Sample>, Vec<String>, RoundCost), String> {
    let connection = database.session();
    warm_through_a_connection(&connection)?;
    let opened = ProcessCost::now();
    let mut samples = Vec::with_capacity(plan.workloads.len());
    for workload in &plan.workloads {
        if let Some(pre) = &workload.pre {
            if let Err(reason) = batch_through_a_connection(&connection, pre) {
                eprintln!("  {}: pre refused: {reason}", workload.name);
                continue;
            }
        }
        match time_one_through_a_connection(&connection, workload, plan.rows) {
            Ok(sample) => samples.push(sample),
            // Absent rather than zero, for the reason `round_on` gives: a
            // sample of zero rolls into its family as an infinitely fast one.
            Err(reason) => eprintln!("  {}: refused: {reason}", workload.name),
        }
        if let Some(post) = &workload.post {
            if let Err(reason) = batch_through_a_connection(&connection, post) {
                eprintln!("  {}: post refused: {reason}", workload.name);
            }
        }
    }
    let mut state = Vec::with_capacity(AGREEMENT.len());
    for question in AGREEMENT {
        let rows = connection
            .query(question)
            .map_err(|error| format!("{question}: {}", why(&error)))?;
        state.push(render_row(&rows));
    }
    Ok((
        samples,
        state,
        // **The costs and the marks are the pipeline arm's to report, and this
        // arm says so by leaving them empty rather than by filling them with
        // numbers about a different object.** `round_on` reads them off the
        // `ImportedDatabase` - `frames_resident`, `wal().stats()`,
        // `pool_stats()` - and a `Connection` is a borrow of a database this
        // function does not hold mutably. The question this arm exists to
        // answer is how long a statement takes through the shipped API, and
        // that is the sample.
        RoundCost {
            round: ProcessCost::now().since(&opened),
            costs: Vec::new(),
            marks: Vec::new(),
        },
    ))
}

/// Fills the pool before the clock starts, the way `round_on` does.
///
/// `ImportedDatabase::warm` is not on the shipped API, so this reads every
/// table the plan touches instead. It is the same effect by the only route a
/// caller has, and it is outside every timed region either way.
///
/// @param connection - the open connection
fn warm_through_a_connection(connection: &ConnectedConnection<'_>) -> Result<(), String> {
    let tables = connection
        .query("SELECT name FROM sqlite_master WHERE type = 'table'")
        .map_err(|error| format!("warming failed: {}", why(&error)))?;
    for row in &tables {
        let Some(OwnedDatum::Text(bytes)) = row.first() else {
            continue;
        };
        let name = String::from_utf8_lossy(bytes).into_owned();
        if name.starts_with("sqlite_") {
            continue;
        }
        // A count reads every page of the table, which is what warming is.
        let _ = connection.query(&format!("SELECT count(*) FROM \"{name}\""));
    }
    Ok(())
}

/// Runs a setup script through the shipped API.
///
/// @param connection - the open connection
/// @param script - the statements, separated by semicolons
fn batch_through_a_connection(
    connection: &ConnectedConnection<'_>,
    script: &str,
) -> Result<(), String> {
    for statement in script.split(';') {
        let trimmed = statement.trim();
        if trimmed.is_empty() {
            continue;
        }
        connection
            .execute(trimmed)
            .map_err(|error| format!("{trimmed}: {}", why(&error)))?;
    }
    Ok(())
}

/// Times one workload through `Connection::prepare` and `Statement::step`.
///
/// **`prepare` is inside the clock for a workload the plan marks
/// `prepare: each` and outside it otherwise**, which is exactly where the
/// pipeline arm puts its `plan`/`prepare` pair and where `sqlite_bench.c` puts
/// `sqlite3_prepare_v2`. A harness that hoisted the compile out of
/// `open.prepare` would be measuring nothing.
///
/// The rows are digested as they are stepped and none are kept, so this arm
/// and the pipeline arm are compared by the same digest over the same values.
///
/// @param connection - the open connection
/// @param workload - the statement and how often to run it
/// @param rows - the fixture's row count, for the bound values
fn time_one_through_a_connection(
    connection: &ConnectedConnection<'_>,
    workload: &Workload,
    rows: u32,
) -> Result<Sample, String> {
    let mut folded = Folded::default();
    let started = Instant::now();
    // **The commits go where the pipeline arm's go.** `time_write` calls
    // `begin_batch` and `commit_batch` at the points `sqlite_bench.c` commits,
    // and an arm that ignored the grouping would be timing two thousand
    // separate transactions against two thousand statements inside one. The
    // first paired run did exactly that and reported `txn.large` at 778x,
    // which is the cost of a file lock and a sync per statement rather than
    // anything the shipped API adds.
    let grouped = workload.grouping != Grouping::Autocommit;
    if grouped {
        connection.execute("BEGIN").map_err(|error| why(&error))?;
    }
    let mut prepared = if workload.prepare_each {
        None
    } else {
        Some(
            connection
                .prepare(&workload.sql)
                .map_err(|error| why(&error))?,
        )
    };
    for iteration in 0..workload.repeat {
        match prepared.as_mut() {
            Some(statement) => {
                statement.reset();
                bind_through_a_statement(statement, workload, iteration, rows)?;
                step_and_digest(statement, &mut folded)?;
            }
            None => {
                // The compile is inside the clock, which is where SQLite's is
                // for a workload the plan marks `prepare: each`.
                let mut statement = connection
                    .prepare(&workload.sql)
                    .map_err(|error| why(&error))?;
                bind_through_a_statement(&mut statement, workload, iteration, rows)?;
                step_and_digest(&mut statement, &mut folded)?;
            }
        }
        if let Grouping::Every(every) = workload.grouping {
            if every > 0 && iteration.saturating_add(1) % every == 0 {
                connection.execute("COMMIT").map_err(|error| why(&error))?;
                if iteration.saturating_add(1) < workload.repeat {
                    connection.execute("BEGIN").map_err(|error| why(&error))?;
                }
            }
        }
    }
    if grouped {
        // **Dropped before the commit**, because a `Statement` borrows the
        // connection and a `COMMIT` through the same connection while one is
        // alive is a statement issued inside another statement's lifetime.
        drop(prepared.take());
        // Autocommit is on again when a `Grouping::Every` closed the last
        // group exactly on the final iteration, and committing then would
        // refuse. The engine's own answer is what decides.
        if !connection.autocommit().map_err(|error| why(&error))? {
            connection.execute("COMMIT").map_err(|error| why(&error))?;
        }
    }
    Ok(Sample {
        workload: workload.name.clone(),
        nanos: started.elapsed().as_secs_f64() * 1e9,
        rows: folded.rows,
        digest: folded.digest.finish(),
    })
}

/// Binds one iteration's values, one-based the way `?1` is.
///
/// @param statement - the prepared statement
/// @param workload - the workload, for what it binds
/// @param iteration - which repeat this is
/// @param rows - the fixture's row count
fn bind_through_a_statement(
    statement: &mut ConnectedStatement<'_>,
    workload: &Workload,
    iteration: u32,
    rows: u32,
) -> Result<(), String> {
    for (index, bind) in workload.binds.iter().enumerate() {
        let at = u32::try_from(index.saturating_add(1)).unwrap_or(1);
        statement
            .bind(at, bind_value(*bind, iteration, rows))
            .map_err(|error| why(&error))?;
    }
    Ok(())
}

/// Steps a statement to the end, digesting every value it produces.
///
/// @param statement - the prepared statement
/// @param folded - the digest and row count to add to
fn step_and_digest(
    statement: &mut ConnectedStatement<'_>,
    folded: &mut Folded,
) -> Result<(), String> {
    while statement.step().map_err(|error| why(&error))? {
        for value in statement.row() {
            eat_borrowed(&mut folded.digest, &value.borrow());
        }
        folded.rows = folded.rows.saturating_add(1);
    }
    Ok(())
}

/// What the second arm needs to run one round, in one value.
///
/// **A struct because `clippy.toml` sets the argument threshold once and
/// `policy.rs` refuses an attribute that moves it for one function.** These
/// four are the same four for every round and none of them changes between
/// rounds, so passing them together is what they are.
struct ArmInputs<'a> {
    /// The pristine SQLite database both arms read.
    fixture: &'a Path,
    /// Where each round's copy goes.
    scratch: &'a Path,
    /// The plan both arms run.
    plan: &'a inillucent_compat::perf::Plan,
    /// The page size, the pool size and which arms were asked for.
    settings: &'a Settings,
}

impl<'a> ArmInputs<'a> {
    /// Gathers what the second arm needs for every round of a run.
    ///
    /// @param fixture - the pristine SQLite database
    /// @param scratch - where each round's copy goes
    /// @param plan - the plan both arms run
    /// @param settings - the page size, the pool size and the arms asked for
    fn new(
        fixture: &'a Path,
        scratch: &'a Path,
        plan: &'a inillucent_compat::perf::Plan,
        settings: &'a Settings,
    ) -> ArmInputs<'a> {
        ArmInputs {
            fixture,
            scratch,
            plan,
            settings,
        }
    }
}

/// Prints the two ways in against each other, when both of them ran.
///
/// @param pairs - one entry per workload
fn report_the_two_ways_in(pairs: &[Paired]) {
    if pairs.is_empty() || report_api_arms(pairs) {
        return;
    }
    println!("  at least one workload is outside the 20% bar; section 4.3.10 asks for the");
    println!("  difference to be explained on docs/performance.md rather than hidden");
}

/// Returns one empty pairing slot per workload, or nothing when only one arm runs.
///
/// **The pairing is what makes the second arm worth having.** The two ways in
/// differ by about thirteen microseconds a statement, and a workload whose
/// whole cost is a few hundred nanoseconds cannot show that against a run of
/// this binary taken at a different time on a machine that moved.
///
/// @param settings - the command line, for which arms were asked for
/// @param plan - the plan, for the workloads
fn api_slots(settings: &Settings, plan: &inillucent_compat::perf::Plan) -> Vec<Paired> {
    if settings.api != Api::Both {
        return Vec::new();
    }
    plan.workloads
        .iter()
        .map(|workload| Paired {
            workload: workload.name.clone(),
            family: workload.family.clone(),
            pairs: Vec::with_capacity(settings.rounds as usize),
            agreed: true,
            disagreement: String::new(),
        })
        .collect()
}

/// Runs the `Connection`-driven arm for one round and pairs it with the pipeline's.
///
/// Does nothing when `pairs` is empty, which is what `--api pipeline` and
/// `--api connection` leave it as.
///
/// @param pairs - the accumulating per-workload pairs
/// @param inputs - the fixture, the scratch area, the plan and the settings
/// @param round - which round this is, for the message
/// @param pipeline - what the pipeline arm produced this round
/// @param pipeline_state - the state questions the pipeline arm left behind
fn run_the_connection_arm(
    pairs: &mut [Paired],
    inputs: &ArmInputs<'_>,
    round: u32,
    pipeline: &[Sample],
    pipeline_state: &[String],
) -> Result<(), String> {
    if pairs.is_empty() {
        return Ok(());
    }
    // After the pipeline arm and the reference, so the connection arm is never
    // the first thing to touch a cold fixture in a round.
    let (through_api, api_state, _) =
        time_through_a_connection(inputs.fixture, inputs.scratch, inputs.plan, inputs.settings)?;
    if api_state != pipeline_state {
        for entry in pairs.iter_mut() {
            entry.agreed = false;
            entry.disagreement = format!(
                "round {round}: the connection arm left {api_state:?} where the pipeline arm                  left {pipeline_state:?}"
            );
        }
        return Ok(());
    }
    pair_the_two_arms(pairs, inputs.plan, pipeline, &through_api);
    Ok(())
}

/// Records one round's engine arm against the reference, workload by workload.
///
/// **Lifted out of [`run`] beside [`pair_the_two_arms`], which is the same
/// shape.** One of the two pairings was a named function and the other was
/// twenty-eight lines inside the round loop, so a reader comparing them had to
/// hold one of them in their head. They now read the same way and differ only
/// where they mean to: a missing sample from this engine is a workload that
/// was refused and is recorded as such, and a missing sample from the
/// reference is a harness fault and stops the run.
///
/// @param measured - one entry per workload, accumulating rounds
/// @param plan - the plan, for the workload order
/// @param ours - what this engine produced this round
/// @param theirs - what the reference produced this round
fn pair_against_the_reference(
    measured: &mut [Paired],
    plan: &inillucent_compat::perf::Plan,
    ours: &[Sample],
    theirs: &[Sample],
) -> Result<(), String> {
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
        // A row-producing workload is compared by its digest. A write produces
        // no rows on either arm, so comparing the digests of two empty result
        // sets proves nothing - what those are compared by is the state
        // questions the caller asks at the end of the round.
        if !workload.mutates && (mine.digest != reference.digest || mine.rows != reference.rows) {
            slot.agreed = false;
            slot.disagreement = format!(
                "inillucent {} rows digest {:016x} against sqlite {} rows digest {:016x}",
                mine.rows, mine.digest, reference.rows, reference.digest
            );
            continue;
        }
        slot.pairs.push((mine.nanos, reference.nanos));
    }
    Ok(())
}

/// Records one round's two engine arms against each other, workload by workload.
///
/// **The digests are compared before the times, the way the reference pairing
/// does it.** Two arms that disagree about the answer are not two measurements
/// of the same thing, and a ratio between them would be a number about a
/// difference nobody has looked at.
///
/// @param pairs - one entry per workload, accumulating rounds
/// @param plan - the plan, for the workload order
/// @param pipeline - what the pipeline arm produced this round
/// @param connection - what the connection arm produced this round
fn pair_the_two_arms(
    pairs: &mut [Paired],
    plan: &inillucent_compat::perf::Plan,
    pipeline: &[Sample],
    connection: &[Sample],
) {
    for (index, workload) in plan.workloads.iter().enumerate() {
        let Some(slot) = pairs.get_mut(index) else {
            continue;
        };
        let (Some(first), Some(second)) = (
            pipeline
                .iter()
                .find(|sample| sample.workload == workload.name),
            connection
                .iter()
                .find(|sample| sample.workload == workload.name),
        ) else {
            slot.agreed = false;
            slot.disagreement = "one of the two arms produced no sample".to_string();
            continue;
        };
        if !workload.mutates && (first.digest != second.digest || first.rows != second.rows) {
            slot.agreed = false;
            slot.disagreement = format!(
                "the pipeline read {} rows digest {:016x} and the connection read {} rows \
                 digest {:016x}",
                first.rows, first.digest, second.rows, second.digest
            );
            continue;
        }
        slot.pairs.push((first.nanos, second.nanos));
    }
}

/// Prints the pipeline arm against the connection arm, and whether they agree
/// to within the bar.
///
/// **Twenty per cent, because that is what §4.3.10 asks for**: the two arms
/// within 20%, or the difference explained on the page. A workload outside it
/// is not a failure of the gate - it is the cost of the shipped API over the
/// shortest path to an answer, and naming it is the point of having the arm.
///
/// The ratio printed is the connection arm over the pipeline arm, so 1.10x
/// means the shipped API costs ten per cent more. That direction is the
/// opposite of the SQLite table's on purpose: this one is a cost and that one
/// is a speedup, and a single column that meant both would be read wrong.
///
/// @param pairs - one entry per workload
/// @returns whether every workload stayed within the bar
fn report_api_arms(pairs: &[Paired]) -> bool {
    /// How far apart the two arms may be before the difference has to be
    /// explained rather than reported (task-2066 §4.3.10).
    const BAR: f64 = 1.20;

    println!();
    println!("## the two ways in: `Connection` over pipeline");
    println!(
        "  {:<24} {:>14} {:>14} {:>9}  within {:.0}%",
        "workload",
        "pipeline ns",
        "connection ns",
        "cost",
        (BAR - 1.0) * 100.0
    );
    let mut every_workload_within = true;
    for entry in pairs {
        if !entry.agreed {
            println!("  {:<24} {}", entry.workload, entry.disagreement);
            every_workload_within = false;
            continue;
        }
        if entry.pairs.is_empty() {
            continue;
        }
        // `medians` names its two sides "ours" and "theirs" because the pairing
        // it was written for is against SQLite. Here the pair is the two ways
        // into this engine, in the order `pair_the_two_arms` pushes them.
        let (pipeline, connection) = entry.medians();
        let cost = if pipeline > 0.0 {
            connection / pipeline
        } else {
            f64::NAN
        };
        let within = cost.is_finite() && cost <= BAR;
        every_workload_within = every_workload_within && within;
        println!(
            "  {:<24} {pipeline:>14.1} {connection:>14.1} {cost:>8.2}x  {}",
            entry.workload,
            if within { "yes" } else { "NO" }
        );
    }
    every_workload_within
}

/// Returns which breakdowns a command line asked for.
///
/// @param settings - what the gate was asked to measure
fn splits_of(settings: &Settings) -> Splits {
    Splits {
        module: settings.module_split,
        put: settings.put_split,
    }
}

/// Which breakdowns this run was asked for.
///
/// **Two flags rather than two bare booleans at three call sites**, because
/// they are passed together through `time_new_engine`, `round_on` and
/// `time_write` and a pair of `bool` arguments in that order is the kind of
/// thing that gets swapped once and reads plausibly afterwards.
#[derive(Clone, Copy, Default)]
struct Splits {
    /// Where a write into a virtual table goes, above the tree.
    module: bool,
    /// Where one row's write into a leaf goes, inside the tree.
    put: bool,
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
/// @param splits - which breakdowns to time and print
fn round_on(
    database: &mut ImportedDatabase,
    plan: &inillucent_compat::perf::Plan,
    splits: Splits,
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
        let pool_before = database.pool_stats();
        let timed = if workload.mutates {
            time_write(database, workload, plan.rows, splits)
        } else {
            time_read(database, workload, plan.rows)
        };
        let spent = ProcessCost::now().since(&before);
        let log_after = database.wal().stats();
        let pool_after = database.pool_stats();
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
                        file_syncs: pool_after.file_syncs.saturating_sub(pool_before.file_syncs),
                        folds: pool_after.folds.saturating_sub(pool_before.folds),
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
    /// Calls to the **data** file's `sync`.
    ///
    /// **What design 1 of task-2000 is graded on.** A commit is one log append and
    /// one sync of the log; the data file is synced only by a fold, twice - once
    /// behind the pages and once behind the meta record. A per-statement number
    /// above zero on `txn.autocommit` means a fold is back on the release path,
    /// which is what took that workload from 1.18 ms to 8.7.
    file_syncs: u64,
    /// Folds: page writes into the data file followed by a meta record.
    folds: u64,
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
/// One row per workload: what it cost this engine in memory, processor time and
/// log traffic.
///
/// **`file sync` and `fold` are task-2000's own columns.** Design 1 makes a commit
/// one log append and one sync and defers the fold, and the claim is about counts
/// rather than milliseconds, so the counts are printed per workload and the medians
/// are over rounds like everything else here. A workload whose `file sync` is one
/// and whose `fold` is zero is a workload where the design is doing what it says.
///
/// Split out of `report_costs` in task-2006, which those two columns took past its
/// recorded length.
///
/// @param plan - the workloads, in the order the report prints them
/// @param ours - one entry per round
fn report_per_workload_costs(plan: &inillucent_compat::perf::Plan, ours: &[RoundCost]) {
    use inillucent_compat::procstat::{mebibytes, millis};
    println!();
    println!("## memory and CPU, this engine, per workload   (median over rounds)");
    println!(
        "  {:<24} {:>12} {:>10} {:>10} {:>8} {:>8} {:>10} {:>9} {:>6}",
        "workload",
        "rss delta MiB",
        "cpu ms",
        "pool frames",
        "log wr",
        "log sync",
        "log KiB",
        "file sync",
        "fold"
    );
    for workload in &plan.workloads {
        let mut rss: Vec<f64> = Vec::new();
        let mut cpu: Vec<f64> = Vec::new();
        let mut frames: Vec<f64> = Vec::new();
        let mut writes: Vec<f64> = Vec::new();
        let mut syncs: Vec<f64> = Vec::new();
        let mut bytes: Vec<f64> = Vec::new();
        let mut file_syncs: Vec<f64> = Vec::new();
        let mut folds: Vec<f64> = Vec::new();
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
            file_syncs.push(log.file_syncs as f64);
            folds.push(log.folds as f64);
        }
        if cpu.is_empty() {
            continue;
        }
        println!(
            "  {:<24} {:>12.2} {:>10.2} {:>10.0} {:>8.0} {:>8.0} {:>10.1} {:>9.0} {:>6.0}",
            workload.name,
            middle(&mut rss),
            middle(&mut cpu),
            middle(&mut frames),
            middle(&mut writes),
            middle(&mut syncs),
            middle(&mut bytes),
            middle(&mut file_syncs),
            middle(&mut folds)
        );
    }
}

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
    report_per_workload_costs(plan, ours);

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
    let started = affinity::spawn_on_same_cores(
        Command::new(exe)
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
            .stderr(std::process::Stdio::piped()),
        "the memory child",
    );
    let mut child = match started {
        Ok(child) => child,
        Err(reason) => {
            eprintln!("  memory child: {reason}");
            return None;
        }
    };
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

    /// Where the last round of each virtual table write workload spent its time.
    ///
    /// Keyed by workload, because two of them write to a module -
    /// `extension.fts.build` and `extension.rtree.insert` - and they go through
    /// the same engine path, which is the reason the split is measured at all:
    /// what it costs is charged to every module and not only to fts5.
    static MODULE_STAGES: std::cell::RefCell<std::collections::BTreeMap<String, String>> =
        const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };

    /// Where the last round of each writing workload's leaf writes spent their time.
    ///
    /// Keyed by workload for the reason `MODULE_STAGES` is, and holding every
    /// family rather than `extension` alone: the question the split exists to
    /// answer is whether an ordinary `INSERT` pays what a shadow row write
    /// pays, and that is two rows of the same table.
    static PUT_STAGES: std::cell::RefCell<std::collections::BTreeMap<String, String>> =
        const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

/// Prints whatever breakdown the last round of one workload recorded.
///
/// Nothing for a workload that records none, so the table stays a table.
///
/// @param workload - the workload just reported
fn print_stage_lines(workload: &str) {
    if workload == "schema.index" {
        let stages = INDEX_STAGES.with(|held| held.borrow().clone());
        if !stages.is_empty() {
            println!("  {:<24} {stages}", "  last round");
        }
    }
    if workload == "extension.fts.build" {
        let stages = FTS_STAGES.with(|held| held.borrow().clone());
        if !stages.is_empty() {
            println!("  {:<24} {stages}", "  last round");
        }
    }
    let split = MODULE_STAGES.with(|held| held.borrow().get(workload).cloned());
    if let Some(split) = split {
        println!("  {:<24} {split}", "  engine path");
    }
    let split = PUT_STAGES.with(|held| held.borrow().get(workload).cloned());
    if let Some(split) = split {
        println!("  {:<24} {split}", "  leaf writes");
    }
}

/// What the gate itself paid around a module workload's statements, in nanoseconds.
///
/// **The first split charged everything outside `execute_statement` to one
/// bucket called the harness, and it was 5.07 us a row against the statement's
/// 8.55** - larger than every engine stage the same run named, and not the
/// gate's parameter binding, which is two string allocations. Three buckets
/// rather than one is what tells the transaction's own commit apart from the
/// binding, and the commit is where it was: `commit_batch` flushes every module
/// and seals the log once for the whole workload, so it is charged per row here
/// and paid once.
#[derive(Clone, Copy, Default)]
struct OutsideStatements {
    /// The total of every `execute_statement` call.
    statement: u128,
    /// Building the parameters each iteration binds.
    binds: u128,
    /// `begin_batch` and `commit_batch`, which happen once for the workload.
    commit: u128,
}

/// Renders what the trees did during one module workload, as one line.
///
/// **The question a stage timing cannot answer: whether a write descended.**
/// Staging rows and writing them as one ordered run at the commit is worth a
/// descent per row, and only if the rows were descending - a rowid append at
/// the right edge already reuses the leaf the hint names, and staging it buys
/// the call overhead and nothing else. `hinted` against `descended` says which
/// of those two a shadow write is before anybody builds the staging.
///
/// @param before - the counters when the clock started
/// @param after - the counters when it stopped
/// @param rows - how many rows the workload wrote
fn tree_work_line(
    before: inillucent_tree::write::WriteStats,
    after: inillucent_tree::write::WriteStats,
    rows: u64,
) -> String {
    if rows == 0 {
        return String::new();
    }
    let ms = |after: u128, before: u128| after.saturating_sub(before) as f64 / 1e6;
    let mut line = format!(
        "hinted {}, descended {}, compactions {}, splits {}",
        after.hinted.saturating_sub(before.hinted),
        after.descended.saturating_sub(before.descended),
        after.compactions.saturating_sub(before.compactions),
        after.splits.saturating_sub(before.splits),
    );
    line.push_str(&format!(
        ", making room {:.2} ms (compact {:.2}, split {:.2})",
        ms(after.room_nanos, before.room_nanos),
        ms(after.compaction_nanos, before.compaction_nanos),
        ms(after.split_nanos, before.split_nanos),
    ));
    line.push_str(&format!(
        ", of the compaction: source {:.2}, image {:.2} (sizing {:.2}, encode {:.2})",
        ms(after.source_nanos, before.source_nanos),
        ms(after.image_nanos, before.image_nanos),
        ms(after.sizing_nanos, before.sizing_nanos),
        ms(after.encode_nanos, before.encode_nanos),
    ));
    line
}

/// Renders where the engine spent its time getting one row to a module, as one line.
///
/// **Microseconds a row rather than milliseconds a workload**, because the bar
/// this is aimed at is stated that way: `extension.fts.build` costs 7.8 us a
/// document to index and 8.4 us to reach the index, against SQLite's 11.5 us for
/// the whole thing.
///
/// `module` is the module's own `update` and should agree with `BuildStages`'
/// `whole` on the line above. `plumbing` is what `change_module` builds around
/// it per row; `values` is the arm's own owned copy of the row; `rest` is what
/// the arm does and does not name; `statement` is everything
/// `execute_statement` did, so `statement` minus `arm` is the plan check, the
/// file lock and the foreign key settle; and `harness` is the workload's own
/// time minus that, which is the gate binding parameters.
///
/// @param stages - what the engine measured
/// @param outside - what the gate paid around the statements
/// @param total - the workload's whole timed region, in nanoseconds
fn module_stage_line(
    stages: inillucent_engine::ModuleStages,
    outside: OutsideStatements,
    total: f64,
) -> String {
    if stages.rows == 0 {
        return String::new();
    }
    let rows = stages.rows as f64;
    let per = |nanos: u128| nanos as f64 / rows / 1e3;
    let plumbing = stages.change.saturating_sub(stages.update);
    let rest = stages
        .whole
        .saturating_sub(stages.values)
        .saturating_sub(stages.change);
    let counted = outside.statement + outside.binds + outside.commit;
    let mut line = format!(
        "{} rows, us/row: statement {:.2}",
        stages.rows,
        per(outside.statement)
    );
    line.push_str(&format!(
        ", arm {:.2} (values {:.2}, plumbing {:.2}, module {:.2}, rest {:.2})",
        per(stages.whole),
        per(stages.values),
        per(plumbing),
        per(stages.update),
        per(rest),
    ));
    line.push_str(&format!(
        ", statement above arm {:.2}, binds {:.2}, commit {:.2}, unattributed {:.2}",
        per(outside.statement.saturating_sub(stages.whole)),
        per(outside.binds),
        per(outside.commit),
        (total - counted as f64) / rows / 1e3,
    ));
    let written = stages.shadow_writes.max(1) as f64;
    line.push_str(&format!(
        "\n  {:<24} {} shadow rows, {:.2} ms writing them",
        "  shadow writes",
        stages.shadow_writes,
        (stages.datums + stages.put) as f64 / 1e6,
    ));
    line.push_str(&format!(
        " ({:.2} ms borrowing the row, {:.2} ms in the tree), {:.2} us a row",
        stages.datums as f64 / 1e6,
        stages.put as f64 / 1e6,
        (stages.datums + stages.put) as f64 / written / 1e3,
    ));
    line
}

/// Renders where one row's write into a leaf went, as two lines.
///
/// **Microseconds a row, and the writes that made room reported apart from the
/// rest.** task-2025 ended at `PagedTree::put` costing 1.8 to 2.2 us for a
/// write that descends nothing, compacts nothing and splits nothing, and an
/// average over every write hides exactly that: 46 of its 1,508 shadow row
/// writes compacted or split and they were the whole of the making of room.
/// The second line is the class the ticket is about.
///
/// `rest` is `whole` minus every stage named, which is the key vector, the key
/// encoding, the two counter updates and the calls themselves. `page write` is
/// the `modify` that writes the row, and `plan` and `place` are inside it, so
/// `page write` minus those two is resolving the frame, marking it dirty and
/// parsing the leaf header.
///
/// @param stages - what the tree measured
/// @param before - the tree counters when the clock started
/// @param after - the tree counters when it stopped
fn put_stage_line(
    stages: inillucent_tree::stages::PutStages,
    before: inillucent_tree::write::WriteStats,
    after: inillucent_tree::write::WriteStats,
) -> String {
    if stages.rows == 0 {
        return String::new();
    }
    let rows = stages.rows as f64;
    let per = |nanos: u128| nanos as f64 / rows / 1e3;
    let ms = |nanos: u128| nanos as f64 / 1e6;
    let named = stages.encode
        + stages.find
        + stages.locate
        + stages.room
        + stages.undo
        + stages.apply
        + stages.making;
    let inside_modify = stages.modify.saturating_sub(stages.plan + stages.delta);
    let mut line = format!(
        "{} writes, {:.2} ms, {:.2} us a write",
        stages.rows,
        ms(stages.whole),
        per(stages.whole),
    );
    line.push_str(&format!(
        "\n  {:<24} us/write: encode {:.2}, find the leaf {:.2}, locate {:.2}, \
         room {:.2}, undo {:.2}, apply {:.2}, make room {:.2}, rest {:.2}",
        "",
        per(stages.encode),
        per(stages.find),
        per(stages.locate),
        per(stages.room),
        per(stages.undo),
        per(stages.apply),
        per(stages.making),
        per(stages.whole.saturating_sub(named)),
    ));
    line.push_str(&format!(
        "\n  {:<24} of locate: pin and parse {:.2}, delta scan {:.2}, binary search {:.2}; \
         of room: the arithmetic {:.2}, the modify around it {:.2}",
        "",
        per(stages.fetch),
        per(stages.deltas),
        per(stages.search),
        per(stages.roomwork),
        per(stages.room.saturating_sub(stages.roomwork)),
    ));
    line.push_str(&format!(
        "\n  {:<24} of apply: extents {:.2}, log record {:.2}, page write {:.2} \
         (plan {:.2}, place {:.2}, the modify itself {:.2})",
        "",
        per(stages.orphans),
        per(stages.logging),
        per(stages.modify),
        per(stages.plan),
        per(stages.delta),
        per(inside_modify),
    ));
    let plain = stages.rows.saturating_sub(stages.remade);
    let plain_nanos = stages.whole.saturating_sub(stages.remade_whole);
    line.push_str(&format!(
        "\n  {:<24} {} made room ({} compactions, {} splits) costing {:.2} ms; \
         the other {} cost {:.2} ms, {:.2} us each",
        "",
        stages.remade,
        after.compactions.saturating_sub(before.compactions),
        after.splits.saturating_sub(before.splits),
        ms(stages.remade_whole),
        plain,
        ms(plain_nanos),
        plain_nanos as f64 / plain.max(1) as f64 / 1e3,
    ));
    line
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
         dict read {:.1} ms, dict write {:.1} ms, totals {:.1} ms, flush {:.1} ms, \
         whole {:.1} ms",
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
        ms(stages.whole),
    )
}

/// Times one mutating workload.
///
/// @param database - the imported fixture, opened for writing
/// @param workload - what to run
/// @param rows - how many rows the base table holds
/// @param splits - which breakdowns to time and print
fn time_write(
    database: &mut ImportedDatabase,
    workload: &Workload,
    rows: u32,
    splits: Splits,
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
    // **The engine's own half of the same measurement.** A workload that writes
    // to a module pays for the arm above the module as well as for the module,
    // and the two are fixed in different crates, so both are reset here and both
    // are printed below.
    let module_split = splits.module && workload.family == "extension" && workload.mutates;
    if module_split {
        database.record_module_stages(true);
    }
    // **Every family, not only `extension`.** `PagedTree::write_row` is the
    // path an ordinary `INSERT` reaches as well as a shadow row, and whether
    // the two microseconds task-2025 left inside it are the write path's or
    // virtual tables' is answered by running the same timer over
    // `write.insert.batch` as over `extension.fts.build`.
    let put_split = splits.put && workload.mutates;
    if put_split {
        inillucent_tree::stages::record_put_stages(true);
    }
    let mut outside = OutsideStatements::default();
    let trees_before = database.write_stats();
    let started = Instant::now();
    if workload.grouping != Grouping::Autocommit {
        database.begin_batch();
    }
    outside.commit = started.elapsed().as_nanos();
    for iteration in 0..workload.repeat {
        // Read only for the two module workloads, because two `Instant::now`
        // calls a row is nothing against 16 us and something against the 1 us a
        // point read costs - and a timer that moves the number it is measuring
        // is how a family gets attributed to the wrong stage.
        let bound = module_split.then(Instant::now);
        let params = params_for(workload, iteration, rows);
        let entered = match bound {
            Some(bound) => {
                outside.binds = outside.binds.saturating_add(bound.elapsed().as_nanos());
                Some(Instant::now())
            }
            None => None,
        };
        let outcome = database
            .execute_statement(&statement, &params)
            .map_err(|error| why(&error))?;
        if let Some(entered) = entered {
            outside.statement = outside
                .statement
                .saturating_add(entered.elapsed().as_nanos());
        }
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
    let sealing = Instant::now();
    database.commit_batch().map_err(|error| why(&error))?;
    outside.commit = outside.commit.saturating_add(sealing.elapsed().as_nanos());
    let nanos = started.elapsed().as_secs_f64() * 1e9;
    if workload.name == "extension.fts.build" {
        let line = fts_stage_line(inillucent_ext::vtab::fts5::build_stages());
        FTS_STAGES.with(|held| *held.borrow_mut() = line);
    }
    if module_split {
        let stages = database.module_stage_nanos();
        let line = module_stage_line(stages, outside, nanos);
        let trees = tree_work_line(trees_before, database.write_stats(), stages.rows);
        database.record_module_stages(false);
        if !line.is_empty() {
            let whole = format!("{line}\n  {:<24} {trees}", "  tree work");
            MODULE_STAGES.with(|held| {
                held.borrow_mut().insert(workload.name.clone(), whole);
            });
        }
    }
    if put_split {
        let stages = inillucent_tree::stages::taken();
        inillucent_tree::stages::record_put_stages(false);
        let line = put_stage_line(stages, trees_before, database.write_stats());
        if !line.is_empty() {
            PUT_STAGES.with(|held| {
                held.borrow_mut().insert(workload.name.clone(), line);
            });
        }
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
    // **Started through the affinity check (task-2085)**: the child inherits
    // this process's mask, and the launcher reads the child's mask back and
    // refuses to time a reference arm running on other processors.
    let mut child = affinity::spawn_on_same_cores(
        Command::new(bench)
            .arg("run")
            .arg(plan)
            .arg(&copy)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()),
        "sqlite-bench",
    )?;
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
