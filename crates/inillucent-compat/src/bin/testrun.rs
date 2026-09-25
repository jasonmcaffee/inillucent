//! The test runner: one build, then every selected binary at once.
//!
//! Invariant: **this runner never decides that a test passed.** It builds what
//! `cargo` builds, runs the same executables `cargo` would run, and reports what
//! they said about themselves. Where it differs from `cargo test` is only in
//! *which* binaries it runs and *how many at a time* - never in what counts as a
//! pass. A runner that could turn a failure into a pass would be worse than no
//! runner.
//!
//! It has **three** answers rather than two. It used to
//! read the process's exit status alone, so `inillucent-bench` - which loads the
//! ONNX runtime and its CUDA provider, and sometimes takes the process down
//! after libtest has printed its summary - was reported as FAILED having passed
//! all 156 of its tests. That needs no load at all to reproduce: three
//! consecutive runs of that binary on its own each printed
//! `test result: ok. 156 passed; 0 failed`, and the first of them exited **127**.
//! So a target either passed, or failed, or **the runner could not tell what it
//! did**, and the third is said out loud with its reason rather than rounded into
//! one of the other two. `inillucent_compat::verdict` is where that reading lives
//! and where its tests are.
//!
//! A target the runner could not read is run **once more, on its own, at the
//! end**, so the answer comes from a second measurement rather than from a guess
//! about which way to round the first. The retry fires only on an absence of
//! information; a target that failed a test is never re-run, because a retry on a
//! failure is how a flaky test stops being noticed.
//!
//! ## Why this exists
//!
//! `cargo test` runs test binaries **one at a time**. That is the right default
//! for a crate, and the wrong one for a workspace with 90 integration suites on
//! a 24-core machine: the suites are processes that mostly wait on the file
//! system, and running them one after another leaves the machine idle for most
//! of a run. Measured on this repository, that is the difference between a full
//! suite you run before a commit and one you do not.
//!
//! It also skips something `cargo test` does not. Of this workspace's 45 bin
//! targets, 43 are benchmark and profiling instruments with no `#[test]` in
//! them, and `cargo test --workspace` starts every one of them to be told it
//! has nothing to run. The runner starts only the two that do hold tests -
//! `inillucent-bench` and `inillucent-shell`, which between them hold 179,
//! because their crates have no library to put them in.
//!
//! That is a claim about every file rather than a habit, so it is checked:
//! `crates/inillucent-compat/tests/tooling/selection.rs` attributes every source file
//! carrying a `#[test]` to the target that compiles it and fails if one lands
//! on a target the map does not name. A test cannot hide in a binary here.
//!
//! ## The three ways to narrow a run
//!
//! - `--changed` asks what the working tree has altered and runs what that can
//!   break, through `inillucent_compat::selection`. This is the pragmatic mode:
//!   a change to one crate does not run the other twenty-eight.
//! - `--tier` runs a named band - `smoke`, `unit`, `engine`, `differential`,
//!   `durability`, `e2e`, `perf`, `tooling`.
//! - `--target` runs one suite by name, for when you already know which.
//!
//! With none of them, it runs every tier whose cadence is `change` or `merge`,
//! which is everything but the `nightly` tier. `--cadence` says which cadence a
//! run is at; `selection::Cadence` says what each one selects and why the
//! nightly and crash suites stopped running on every change.
//!
//! ## The failure this runner is most careful about
//!
//! Several suites need something the workspace cannot build: the pinned SQLite
//! oracle, a C compiler, the ONNX runtime. Each of them returns early and
//! reports success when its prerequisite is absent, which is correct behaviour
//! for `cargo test` on a fresh clone and a trap for anybody reading a green
//! run as evidence. So the runner counts them, names them, and `--strict`
//! turns the count into a non-zero exit. A run that evidenced nothing must not
//! look like a pass.
//!
//! The same rule reaches the exit code itself. A run that never started - a
//! build that did not compile, a named selection that matched nothing, an MSVC
//! environment that is not there - exits **2** rather than 1, so that "nothing
//! was graded" is a state a caller can branch on instead of a sentence it has
//! to find in the log. `main` has the three codes and why they are three.
//!
//! ## Building it
//!
//! ```text
//! cargo build -p inillucent-compat --bin inillucent-testrun --features testrun
//! target/debug/inillucent-testrun --tier smoke
//! ```
//!
//! The feature is not optional decoration and `cargo run` will not work in its
//! place. Cargo builds a package's binaries whenever that package has
//! integration tests, so an ordinary `cargo test --no-run` would try to replace
//! *this* executable while it was running it - which Windows refuses, and the
//! run dies on its own binary. `required-features` keeps the runner out of the
//! build the runner starts, which makes that impossible rather than unlikely.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use inillucent_compat::layering;
use inillucent_compat::selection::{self, Cadence, Kind, Map, Row, Target};
use inillucent_compat::supervise::{self, Limits, Stopped};
use inillucent_compat::testplan::{self, json_field, json_text, Artifact};
use inillucent_compat::verdict::{self, Undetermined, Verdict};
use inillucent_compat::workspace_root;
use inillucent_scalar::json::node::Node;
use inillucent_scalar::json::parse;

/// How long a target is assumed to take when the ledger has never seen it.
///
/// Longest-first scheduling needs a number for every target, and a new suite
/// has none. Assuming it is slow is the safe direction: a slow target started
/// late is what leaves one core working after the other twenty-three have run
/// out of work, and that tail is the whole cost of getting the order wrong.
const UNKNOWN_MILLISECONDS: u64 = 30_000;

/// What the command line asked for.
struct Options {
    /// Tiers to run; empty means every tier.
    tiers: Vec<String>,
    /// Targets to run by label; empty means no explicit choice.
    targets: Vec<String>,
    /// Select from the working tree's changes against this revision.
    changed: Option<String>,
    /// Which cadence to run at; `None` is `change` with `--changed` and
    /// `merge` without it. See `selection::Cadence`.
    cadence: Option<Cadence>,
    /// How many test binaries to run at once.
    jobs: usize,
    /// How many threads each test binary uses internally.
    test_threads: usize,
    /// Print the selection and stop.
    list: bool,
    /// Print the tiers and stop.
    list_tiers: bool,
    /// Treat a missing prerequisite as a failure.
    strict: bool,
    /// Prerequisites this machine declares it cannot have, from `--absent`.
    ///
    /// Added to what `tests/prerequisites.local.toml` declares. A suite that
    /// skipped for want of one of these is reported under its own heading and
    /// does not make a strict run red. Every other missing prerequisite still
    /// does. CI passes them here, per runner, because it has no local file.
    absent: Vec<String>,
    /// Write the measured times back to the ledger.
    record: bool,
    /// Skip the build step, because the caller has just built.
    no_build: bool,
    /// Read what was built from this artifact list and start no cargo at all.
    artifacts: Option<PathBuf>,
    /// Write the verdict as JSON here, for the nightly and the release.
    summary: Option<PathBuf>,
    /// Pass a filter through to each test binary.
    filter: Option<String>,
    /// What `--timeout` asked for, in seconds.
    ///
    /// `None` works each target's budget out from its own recorded time, which
    /// is what an ordinary run wants. `Some(0)` turns the budget off and leaves
    /// only the half of this that needs no judgement: reporting a target whose
    /// child has exited instead of waiting on its pipe.
    timeout: Option<u64>,
}

impl Options {
    /// Copies the options, so the exclusive pass can differ in how much of the
    /// machine it takes and in nothing else.
    ///
    /// @param other - the options to copy
    fn from(other: &Options) -> Options {
        Options {
            tiers: other.tiers.clone(),
            targets: other.targets.clone(),
            changed: other.changed.clone(),
            cadence: other.cadence,
            jobs: other.jobs,
            test_threads: other.test_threads,
            list: other.list,
            list_tiers: other.list_tiers,
            strict: other.strict,
            absent: other.absent.clone(),
            record: other.record,
            no_build: other.no_build,
            artifacts: other.artifacts.clone(),
            summary: other.summary.clone(),
            filter: other.filter.clone(),
            timeout: other.timeout,
        }
    }
}

impl Default for Options {
    fn default() -> Options {
        Options {
            tiers: Vec::new(),
            targets: Vec::new(),
            changed: None,
            cadence: None,
            jobs: std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(4),
            // Two, not one, and not the default. Each binary already runs its
            // own tests on several threads; multiplying that by the number of
            // binaries in flight oversubscribes the machine badly enough to be
            // slower than running them one at a time. Two keeps a little
            // intra-binary parallelism without the run becoming a thundering
            // herd - see the TDD's scheduling section.
            test_threads: 2,
            list: false,
            list_tiers: false,
            strict: false,
            absent: Vec::new(),
            record: false,
            no_build: false,
            artifacts: None,
            summary: None,
            filter: None,
            timeout: None,
        }
    }
}

/// Reads the command line, reporting anything it does not understand.
///
/// @param arguments - the arguments after the program name
fn parse_options(arguments: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0usize;
    while let Some(argument) = arguments.get(index) {
        index += 1;
        let mut take = |name: &str| -> Result<String, String> {
            let value = arguments.get(index).cloned();
            index += 1;
            value.ok_or_else(|| format!("`{name}` needs a value"))
        };
        match argument.as_str() {
            "--tier" => options.tiers.push(take("--tier")?),
            "--target" => options.targets.push(take("--target")?),
            "--jobs" => {
                let value = take("--jobs")?;
                options.jobs = value
                    .parse()
                    .map_err(|_| format!("`--jobs` wants a number, not `{value}`"))?;
            }
            "--test-threads" => {
                let value = take("--test-threads")?;
                options.test_threads = value
                    .parse()
                    .map_err(|_| format!("`--test-threads` wants a number, not `{value}`"))?;
            }
            "--filter" => options.filter = Some(take("--filter")?),
            "--timeout" => {
                let value = take("--timeout")?;
                options.timeout = Some(
                    value
                        .parse()
                        .map_err(|_| format!("`--timeout` wants seconds, not `{value}`"))?,
                );
            }
            "--changed" => {
                // The revision is optional: `--changed` alone means the working
                // tree against HEAD, which is what somebody about to commit
                // wants.
                match arguments.get(index) {
                    Some(next) if !next.starts_with("--") => {
                        options.changed = Some(next.clone());
                        index += 1;
                    }
                    _ => options.changed = Some("HEAD".to_string()),
                }
            }
            "--cadence" => {
                let value = take("--cadence")?;
                options.cadence = Some(Cadence::parse(&value).ok_or_else(|| {
                    format!("`--cadence` wants change, merge or nightly, not `{value}`")
                })?);
            }
            "--all" => {
                options.tiers.clear();
                options.targets.clear();
                options.changed = None;
            }
            "--list" => options.list = true,
            "--list-tiers" => options.list_tiers = true,
            "--strict" => options.strict = true,
            "--absent" => options.absent.push(take("--absent")?),
            "--record" => options.record = true,
            "--no-build" => options.no_build = true,
            "--artifacts" => options.artifacts = Some(PathBuf::from(take("--artifacts")?)),
            "--summary" => options.summary = Some(PathBuf::from(take("--summary")?)),
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown option `{other}`\n\n{}", usage())),
        }
        if options.jobs == 0 {
            return Err("`--jobs` must be at least 1".to_string());
        }
    }
    Ok(options)
}

/// The help text.
fn usage() -> String {
    "inillucent-testrun - run the workspace's tests in parallel\n\
     \n\
     Narrowing (with none of these, every change and merge tier runs):\n  \
       --changed [<rev>]   run what the changes since <rev> (default HEAD) can break\n  \
       --cadence <when>    change, merge or nightly: the highest cadence to run.\n                      \
       With --changed the default is change: merge tiers run only for\n                      \
       a changed crate they cover, nightly tiers never. Without it the\n                      \
       default is merge: everything but the nightly tier\n  \
       --tier <name>       run one tier; repeatable\n  \
       --target <label>    run one target as `package::name`; repeatable\n  \
       --filter <text>     pass a name filter to each test binary\n\
     \n\
     Execution:\n  \
       --jobs <n>          test binaries at once (default: the machine's cores)\n  \
       --test-threads <n>  threads inside each binary (default: 2)\n  \
       --no-build          do not build first\n  \
       --artifacts <file>  run the executables this artifact list names and start no\n                      \
       cargo; every run writes one and names it in INILLUCENT_TESTRUN_ARTIFACTS\n  \
       --timeout <secs>    how long one target may run; 0 never stops one\n\
     \n\
     Reporting:\n  \
       --list              print the selection and stop\n  \
       --list-tiers        print the tiers and stop\n  \
       --strict            fail when a selected suite's prerequisite is missing\n  \
       --absent <name>     a prerequisite this machine cannot have: a suite that\n                      \
       skips for it does not fail --strict; repeatable\n  \
       --record            write the measured times to tests/timings.toml\n  \
       --summary <file>    write the verdict, the failures and the declared absences as JSON\n\
     \n\
     Declared absences: a strict run does not fail for a suite whose every missing\n\
     prerequisite is listed in the gitignored tests/prerequisites.local.toml as\n\
     `absent = [\"mysql\", ...]`. It reports those suites under their own heading.\n\
     \n\
     Exit codes:\n  \
       0                   everything selected ran and passed\n  \
       1                   the run happened and was red\n  \
       2                   the run did not happen: nothing was graded\n\
     \n\
     A target is stopped only when it is BOTH past its budget and has\n\
     printed nothing for ten minutes, because a suite that shells out to\n\
     the pinned SQLite oracle is legitimately silent for a long time. The\n\
     budget is eight times what tests/timings.toml recorded for that\n\
     target, and never under two hours. `--timeout <secs>` replaces\n\
     it, and `--timeout 0` removes it.\n\
     \n\
     Read the code, not the last line. In a shell, `$?` after a pipeline is the\n\
     status of the last command in it, so `inillucent-testrun | tail` reports\n\
     tail's 0 however the run went.\n"
        .to_string()
}

/// One built test binary.
///
/// Cloned rather than moved into its worker, because a target whose first run
/// could not be read is started again at the end of the pass.
#[derive(Clone)]
struct Built {
    /// Which target it is.
    target: Target,
    /// Where the executable is.
    executable: PathBuf,
    /// The directory `cargo` would run it from.
    directory: PathBuf,
    /// The test names to pass with `--exact`, for a target that is one module
    /// of a binary; `None` runs the whole binary.
    tests: Option<Vec<String>>,
}

/// What one finished run produced.
struct Outcome {
    /// Which target ran.
    target: Target,
    /// What it did, read from its transcript and its exit status together.
    verdict: Verdict,
    /// How long it took.
    elapsed: Duration,
    /// How many tests ran, as the harness counted them.
    ran: usize,
    /// Everything it printed, kept for the report when it did not pass.
    output: String,
    /// How it exited, for the report.
    ///
    /// A harness that prints `test result: ok` and then exits non-zero is not a
    /// contradiction to be discarded: it is a process that died after its tests
    /// passed - in a destructor, in a background thread, or at the hands of the
    /// operating system. Without the status in the report, that failure has no
    /// visible cause at all.
    status: String,
    /// What the first attempt looked like, when this outcome is a second one.
    ///
    /// Printed whether the retry settled the question or not: a target that
    /// passes its retry still died once, and a report that hid that would be the
    /// same silence this runner was fixed for.
    retry_of: Option<String>,
}

/// The exit code for a run that did not happen. See `main`.
///
/// A function rather than a `const` because `ExitCode::from` is not const.
fn did_not_run() -> ExitCode {
    ExitCode::from(2)
}

/// Runs the whole thing.
///
/// **Three exit codes, because "the run was red" and "the run did not happen"
/// are different facts (task-2047).** A build that does not compile printed
/// `the build failed` and exited 1, which is the code a failing test exits, so
/// nothing reading the status could tell a broken toolchain from a real defect.
/// An agent read one as the other and had to go back through 60 KB of log to
/// find out. The command line already spends a third code on `unsupported` for
/// the same reason: a caller should be able to branch without matching on a
/// message.
///
/// - `0` - every selected target ran and passed.
/// - `1` - the run happened and was red: a target failed, a target was still
///   undetermined after its second attempt, or `--strict` found a suite whose
///   prerequisite was absent.
/// - `2` - the run did not happen. The command line was wrong, a named
///   selection matched nothing, the MSVC environment could not be found, the
///   build failed, or cargo could not say what it had built. Nothing was
///   graded, so nothing here may be read as a pass.
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse_options(&arguments) {
        Ok(options) => options,
        Err(reason) => {
            eprintln!("{reason}");
            return did_not_run();
        }
    };
    match run(&options) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(reason) => {
            eprintln!("inillucent-testrun: {reason}");
            did_not_run()
        }
    }
}

/// Selects, builds, runs and reports. Returns whether everything passed.
///
/// @param options - what the command line asked for
fn run(options: &Options) -> Result<bool, String> {
    let root = workspace_root();
    let map = Map::load(&root.join("tests/selection.toml"))?;
    // Read before anything runs, so a misspelt name refuses the run instead
    // of excusing nothing at the end of it.
    let declared = declared_absent(&root, &map, &options.absent)?;

    if options.list_tiers {
        print_tiers(&map);
        return Ok(true);
    }

    let selected = choose(&root, &map, options)?;
    if selected.is_empty() {
        return empty_selection(options);
    }

    if options.list {
        print_selection(&selected);
        return Ok(true);
    }

    let built = executables_for(&root, &map, &selected, options)?;
    let ledger = read_ledger(&root.join("tests/timings.toml"));
    // Kept by target so the retry pass can start one again. The workers take
    // their own clone, so nothing here is a second opinion about what was built.
    let executables: BTreeMap<Target, Built> = built
        .iter()
        .map(|one| (one.target.clone(), one.clone()))
        .collect();
    let ordered = schedule(built, &ledger);
    let budgets = budgets(&executables, &ledger, options);

    let (shared, alone) = split_alone(&map, ordered);

    println!(
        "running {} target(s), {} at a time, {} thread(s) each{}",
        shared.len() + alone.len(),
        options.jobs,
        options.test_threads,
        if alone.is_empty() {
            String::new()
        } else {
            format!(
                "; {} of them alone at the end, one thread each",
                alone.len()
            )
        }
    );
    let started = Instant::now();
    let mut outcomes = execute(shared, options, &budgets);
    if !alone.is_empty() {
        // **One binary at a time, and one thread inside it.** `jobs: 1` alone
        // gave an exclusive target the machine to itself among the *binaries*
        // and then still ran its own tests two at a time against each other -
        // so `inillucent::budget`, whose six guards are the reason this tier
        // exists, was measuring one guard while another ran beside it. That is
        // the same contention the tier is declared exclusive to avoid, at a
        // smaller scale, and it is the half the tier can actually fix.
        let solo = Options {
            jobs: 1,
            test_threads: 1,
            ..Options::from(options)
        };
        outcomes.extend(execute(alone, &solo, &budgets));
    }
    let outcomes = settle_undetermined(outcomes, &executables, options, &budgets);
    let wall = started.elapsed();

    let absences = absences(&outcomes, &map, &declared);
    report(&outcomes, wall, options.strict, &absences);
    if let Some(file) = &options.summary {
        // Red rather than exit 2, for the reason `--record` gives below: the
        // run happened, and a summary nobody can read is a run nobody can act on.
        if let Err(reason) = write_summary(file, &outcomes, wall, options.strict, &absences) {
            eprintln!("inillucent-testrun: the summary was not written: {reason}");
            return Ok(false);
        }
    }
    if options.record {
        // **A ledger that could not be written is not "the run did not happen"
        // (task-2047).** The targets ran and the report above is what they
        // said, so returning `Err` here would give that run exit code 2 and
        // throw its verdict away. Saying so and keeping the run red records the
        // failure without pretending nothing was graded.
        match write_ledger(&root.join("tests/timings.toml"), &ledger, &outcomes) {
            Ok(()) => println!("recorded {} timing(s)", outcomes.len()),
            Err(reason) => {
                eprintln!("inillucent-testrun: the timings were not recorded: {reason}");
                return Ok(false);
            }
        }
    }

    // Undetermined counts as red, and deliberately so. The run is a gate, and
    // "the runner could not tell" is not evidence that anything passed. What
    // stops that being the old wrong red is the retry above: a target only stays
    // undetermined here when a second, solitary attempt could not read it either.
    //
    // A target whose every failure is a strict skip is not red: it evidenced
    // nothing, and `hollow` is where that is graded, which is the one place a
    // declared absence can excuse it.
    let red = outcomes.iter().any(is_red);
    let hollow = options.strict && !absences.unexcused.is_empty();
    if !red && nothing_was_graded(&outcomes) {
        return Err(graded_nothing(options, outcomes.len()));
    }
    Ok(!red && !hollow)
}

/// Prints the tiers, their cadence and what each is for.
///
/// @param map - the selection map
fn print_tiers(map: &Map) {
    for tier in &map.tiers {
        println!(
            "{:<12} {:<8} {}{}",
            tier.name,
            tier.cadence.as_str(),
            tier.purpose,
            if tier.exclusive { "  [runs alone]" } else { "" }
        );
    }
}

/// Prints the selection, one target per line, for `--list`.
///
/// @param selected - the rows the run would start
fn print_selection(selected: &[&Row]) {
    for row in selected {
        println!(
            "{:<10} {:<44} covers {}",
            row.tier,
            row.target.label(),
            row.covers.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    println!(
        "
{} target(s)",
        selected.len()
    );
}

/// Returns the executable for every selected row, building them first unless
/// the run was handed an artifact list.
///
/// **A nested run starts no cargo (task-2114 C2).** Given `--artifacts`, the
/// executables come from the list the outer runner wrote. Otherwise the rows
/// are built, located, and the list is written for any nested run a suite
/// starts. Either way `INILLUCENT_TESTRUN_ARTIFACTS` names the list for the
/// children.
///
/// @param root - the workspace root
/// @param map - the selection map
/// @param selected - the rows the run will start
/// @param options - what the command line asked for
fn executables_for(
    root: &Path,
    map: &Map,
    selected: &[&Row],
    options: &Options,
) -> Result<Vec<Built>, String> {
    if let Some(file) = &options.artifacts {
        std::env::set_var(testplan::ARTIFACTS_VARIABLE, file);
        let mut built = pick(&read_artifact_list(file)?, selected)?;
        list_module_tests(&mut built, options.filter.as_deref())?;
        return Ok(built);
    }
    let to_build = testplan::build_set(map, selected);
    // Before either cargo call, because `locate` compiles too: it asks cargo
    // what it built, and cargo answers that by building.
    import_msvc_environment()?;
    if !options.no_build {
        build(root, &to_build)?;
    }
    let located = locate(root, &to_build)?;
    let file = write_artifact_list(root, &located)?;
    std::env::set_var(testplan::ARTIFACTS_VARIABLE, &file);
    let mut built = pick(&located, selected)?;
    list_module_tests(&mut built, options.filter.as_deref())?;
    Ok(built)
}

/// Fills in each module target's test names, listing each binary once.
///
/// A listing takes milliseconds, and it is what lets one binary per tier run
/// one suite at a time: the module's own names go to `--exact`, so the process
/// runs that suite and no other.
///
/// @param built - the targets, some of them modules
/// @param filter - the runner's `--filter`, if any
fn list_module_tests(built: &mut [Built], filter: Option<&str>) -> Result<(), String> {
    let mut listings: BTreeMap<PathBuf, String> = BTreeMap::new();
    for one in built.iter_mut() {
        let Some(module) = one.target.module.clone() else {
            continue;
        };
        if !listings.contains_key(&one.executable) {
            let output = Command::new(&one.executable)
                .current_dir(&one.directory)
                .args(["--list", "--format", "terse"])
                .stderr(Stdio::inherit())
                .output()
                .map_err(|error| format!("cannot list {}: {error}", one.executable.display()))?;
            if !output.status.success() {
                return Err(format!(
                    "{} could not list its tests",
                    one.executable.display()
                ));
            }
            listings.insert(
                one.executable.clone(),
                String::from_utf8_lossy(&output.stdout).into_owned(),
            );
        }
        let listing = listings
            .get(&one.executable)
            .map(String::as_str)
            .unwrap_or("");
        one.tests = Some(testplan::module_tests(listing, &module, filter));
    }
    Ok(())
}

/// Splits the targets into those that share the machine and those that run
/// alone at the end.
///
/// A target runs alone because its tier asked for the machine, or because the
/// row itself did. See `selection::Tier::exclusive` for why a timing guard
/// cannot share a machine, and why widening its threshold instead would only
/// have made it stop guarding.
///
/// @param map - the selection map
/// @param ordered - the targets, longest first
fn split_alone(map: &Map, ordered: Vec<Built>) -> (Vec<Built>, Vec<Built>) {
    let exclusive_tiers: BTreeSet<&str> = map
        .tiers
        .iter()
        .filter(|tier| tier.exclusive)
        .map(|tier| tier.name.as_str())
        .collect();
    let mut shared = Vec::new();
    let mut alone = Vec::new();
    for built in ordered {
        let solo = map
            .row(&built.target)
            .is_some_and(|row| exclusive_tiers.contains(row.tier.as_str()) || row.alone);
        if solo {
            alone.push(built);
        } else {
            shared.push(built);
        }
    }
    (shared, alone)
}

/// Returns what this machine declares absent: the local file and `--absent`.
///
/// A name no row of the map requires is refused, which is exit code 2. A
/// misspelt name would otherwise excuse nothing and read, in the workflow or
/// the file that names it, as if it excused something.
///
/// @param root - the workspace root
/// @param map - the selection map, whose rows name every prerequisite
/// @param absent - what `--absent` named
fn declared_absent(root: &Path, map: &Map, absent: &[String]) -> Result<Vec<String>, String> {
    let mut declared = testplan::read_declared_absences(root)?;
    for name in absent {
        if !declared.contains(name) {
            declared.push(name.clone());
        }
    }
    declared.sort();
    let known = map.prerequisites();
    let unknown: Vec<String> = declared
        .iter()
        .filter(|name| !known.contains(*name))
        .map(|name| format!("`{name}`"))
        .collect();
    if unknown.is_empty() {
        return Ok(declared);
    }
    Err(format!(
        "{} declared absent, by `--absent` or in {}, and no row of tests/selection.toml \
         requires it. The names accepted are: {}",
        unknown.join(", "),
        testplan::DECLARED_ABSENCES_FILE,
        known.into_iter().collect::<Vec<_>>().join(", ")
    ))
}

/// Reports whether a target found a defect or could not be read.
///
/// A target that failed only because `--strict` turned its skips into failed
/// tests is not red: it evidenced nothing, and `missing_prerequisites` is what
/// counts it, where a declared absence can excuse it. Everything else that is
/// not green is red, undetermined included.
///
/// @param outcome - the target that ran
fn is_red(outcome: &Outcome) -> bool {
    if outcome.verdict.is_green() {
        return false;
    }
    !(matches!(outcome.verdict, Verdict::Failed)
        && inillucent_compat::differential::every_failure_is_a_strict_skip(
            &outcome.output,
            failed_count(&outcome.output),
        ))
}

/// The targets that ran without a prerequisite, split by whether the machine
/// declared that prerequisite absent.
struct Absences<'run> {
    /// Hollow targets nothing excuses; `--strict` fails the run for these.
    unexcused: Vec<(&'run Outcome, Vec<String>)>,
    /// Hollow targets whose every missing thing is declared absent.
    excused: Vec<(&'run Outcome, Vec<String>)>,
    /// What `tests/prerequisites.local.toml` declares, for the report.
    declared: Vec<String>,
    /// How many selected targets declare a prerequisite at all.
    declaring: usize,
}

/// Writes the verdict as JSON, for a program that has to act on it.
///
/// The nightly reads this to decide green or red and to name the failing
/// targets in the ticket it files, and the release reads the nightly's copy.
/// Parsing the text report instead would make its wording an interface.
///
/// `result` is `red` when a target failed or could not be read, `hollow` when
/// nothing failed and a strict run found a suite without its prerequisite that
/// no declaration excuses, and `green` otherwise.
///
/// @param file - where to write it
/// @param outcomes - what ran
/// @param wall - how long the run took
/// @param strict - whether this was a strict run
/// @param absences - the hollow targets, split by the declaration
fn write_summary(
    file: &Path,
    outcomes: &[Outcome],
    wall: Duration,
    strict: bool,
    absences: &Absences,
) -> Result<(), String> {
    let strictly_skipped = |outcome: &Outcome| {
        inillucent_compat::differential::every_failure_is_a_strict_skip(
            &outcome.output,
            failed_count(&outcome.output),
        )
    };
    let failed: Vec<String> = outcomes
        .iter()
        .filter(|outcome| matches!(outcome.verdict, Verdict::Failed) && !strictly_skipped(outcome))
        .map(|outcome| testplan::json_string(&outcome.target.label()))
        .collect();
    let undetermined: Vec<String> = outcomes
        .iter()
        .filter(|outcome| outcome.verdict.undetermined().is_some())
        .map(|outcome| testplan::json_string(&outcome.target.label()))
        .collect();
    let listed = |entries: &[(&Outcome, Vec<String>)]| -> String {
        entries
            .iter()
            .map(|(outcome, said)| {
                format!(
                    "{{\"target\": {}, \"missing\": [{}]}}",
                    testplan::json_string(&outcome.target.label()),
                    said.iter()
                        .map(|entry| testplan::json_string(entry))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let result = if !failed.is_empty() || !undetermined.is_empty() {
        "red"
    } else if strict && !absences.unexcused.is_empty() {
        "hollow"
    } else {
        "green"
    };
    let text = format!(
        "{{\n  \"result\": \"{result}\",\n  \"strict\": {strict},\n  \"targets\": {},\n  \
         \"tests\": {},\n  \"wall_seconds\": {:.1},\n  \"failed\": [{}],\n  \
         \"undetermined\": [{}],\n  \"hollow\": [{}],\n  \"declared_absent\": [{}],\n  \
         \"not_evidenced_by_declaration\": [{}]\n}}\n",
        outcomes.len(),
        outcomes.iter().map(|outcome| outcome.ran).sum::<usize>(),
        wall.as_secs_f64(),
        failed.join(", "),
        undetermined.join(", "),
        listed(&absences.unexcused),
        absences
            .declared
            .iter()
            .map(|name| testplan::json_string(name))
            .collect::<Vec<_>>()
            .join(", "),
        listed(&absences.excused),
    );
    if let Some(parent) = file.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
    }
    std::fs::write(file, text).map_err(|error| format!("cannot write {}: {error}", file.display()))
}

/// Sorts the hollow targets into excused and not.
///
/// @param outcomes - what ran
/// @param map - the selection map, for what each target requires
/// @param declared - what this machine declares absent
fn absences<'run>(outcomes: &'run [Outcome], map: &Map, declared: &[String]) -> Absences<'run> {
    let mut split = Absences {
        unexcused: Vec::new(),
        excused: Vec::new(),
        declared: declared.to_vec(),
        declaring: outcomes
            .iter()
            .filter(|outcome| {
                map.row(&outcome.target)
                    .is_some_and(|row| !row.requires.is_empty())
            })
            .count(),
    };
    for (outcome, said) in missing_prerequisites(outcomes, map) {
        // Matched on the row, not on the suite's own skip sentence: the
        // sentence is prose and the row is a name. A suite requiring two
        // things, one of them declared, is excused whichever it was missing,
        // which is why a declaration is for what a machine cannot have and not
        // for what a setup step forgot to build.
        if map
            .row(&outcome.target)
            .is_some_and(|row| row.needs_any_of(declared))
        {
            split.excused.push((outcome, said));
        } else {
            split.unexcused.push((outcome, said));
        }
    }
    split
}

/// Returns whether every target that ran counted no test.
///
/// Read by `report` as well as by `run`, so the word the report ends on and the
/// code the process exits with come from the same question. The comment on
/// `report`'s `ok` says why that matters: a report and an exit status that
/// disagree end with one of the two being ignored.
///
/// @param outcomes - what the run produced
fn nothing_was_graded(outcomes: &[Outcome]) -> bool {
    !outcomes.is_empty() && outcomes.iter().all(|outcome| outcome.ran == 0)
}

/// Refuses a run whose binaries all started and graded no test.
///
/// **`--filter` with a name no test carries prints `ok` and exits zero
/// (task-2047).** Every selected binary really did run, so no verdict is red,
/// and the summary line reads `1 target(s), 0 test(s), 0 failed, 0
/// undetermined` above a green word. It is the build-that-did-not-happen
/// wearing a report: no test was graded, so there is nothing in the run to
/// pass on, and `--filter` takes free text so a typo in a test name reaches it
/// on the first try.
///
/// Zero across a whole run is always the filter and never the map.
/// `selection.rs`'s `every_target_has_a_row` attributes every source file
/// holding a `#[test]` to the target that compiles it, so a target the map
/// names has tests in it by construction.
///
/// Checked only when nothing is red: a binary that died before its first test
/// also ran none, and that is an undetermined target with its own reason,
/// which the report has already said better than this sentence would.
///
/// @param options - what the command line asked for
/// @param targets - how many targets ran
fn graded_nothing(options: &Options, targets: usize) -> String {
    match &options.filter {
        Some(filter) => format!(
            "{targets} target(s) ran and no test matched `--filter {filter}`, so nothing was \
             graded"
        ),
        None => format!("{targets} target(s) ran and graded no test at all"),
    }
}

/// Works out which rows to run.
///
/// @param root - the workspace root
/// @param map - the selection map
/// @param options - what the command line asked for
fn choose<'map>(root: &Path, map: &'map Map, options: &Options) -> Result<Vec<&'map Row>, String> {
    let cadence = run_cadence(map, options);
    let mut rows: Vec<&Row> = if let Some(revision) = &options.changed {
        let members = layering::workspace_members(root)?;
        let manifests = layering::read_members(root, &members)?;
        let changed = changed_paths(root, revision)?;
        if changed.is_empty() {
            println!("nothing has changed against {revision}");
            return Ok(Vec::new());
        }
        let graph = selection::dependents(&manifests);
        let choice = selection::seeds_of(map, &members, &manifests, &changed);
        if choice.selects_everything {
            let why = if choice.unmatched.is_empty() {
                "a changed path is declared to select everything".to_string()
            } else {
                format!(
                    "no rule covers {}, so everything is selected",
                    choice.unmatched.join(", ")
                )
            };
            println!("{} changed path(s): {why}", changed.len());
        } else {
            println!(
                "{} changed path(s) in {}; {} package(s) affected",
                changed.len(),
                choice.seeds.iter().cloned().collect::<Vec<_>>().join(", "),
                selection::affected(&graph, &choice.seeds).len()
            );
        }
        selection::select_at(map, &choice, &graph, cadence)
    } else {
        map.rows_up_to(cadence)
    };

    if !options.targets.is_empty() {
        let wanted: BTreeSet<&str> = options.targets.iter().map(String::as_str).collect();
        rows.retain(|row| wanted.contains(row.target.label().as_str()));
        for want in &wanted {
            if !map.rows.iter().any(|row| row.target.label() == *want) {
                return Err(format!("no target called `{want}`"));
            }
        }
    }
    if !options.tiers.is_empty() {
        let wanted: BTreeSet<&str> = options.tiers.iter().map(String::as_str).collect();
        for want in &wanted {
            if !map.tiers.iter().any(|tier| tier.name == *want) {
                return Err(format!("no tier called `{want}`; try --list-tiers"));
            }
        }
        rows.retain(|row| wanted.contains(row.tier.as_str()));
    }
    Ok(rows)
}

/// Returns the cadence this run selects at.
///
/// `--cadence` when given. Otherwise `change` for `--changed`, which is the
/// ticket loop, and `merge` for a run with no `--changed`, which is everything
/// but the nightly tier. A tier or a target named on the command line raises it
/// to that tier's own cadence, so `--tier nightly` still runs the nightly tier
/// and `--target` still runs a nightly story by name.
///
/// @param map - the selection map
/// @param options - what the command line asked for
fn run_cadence(map: &Map, options: &Options) -> Cadence {
    let mut cadence = options.cadence.unwrap_or(if options.changed.is_some() {
        Cadence::Change
    } else {
        Cadence::Merge
    });
    for tier in &options.tiers {
        cadence = cadence.max(map.cadence_of(tier));
    }
    for row in &map.rows {
        if options.targets.contains(&row.target.label()) {
            cadence = cadence.max(map.cadence_of(&row.tier));
        }
    }
    cadence
}

/// Answers a selection that came back empty.
///
/// **A request that could not be honoured is not a passing run (task-2047).**
/// `--target inillucent-compat::engine::dml --tier smoke` names a real target and a
/// real tier, so neither guard in `choose` fires - those catch a name the map
/// does not hold at all - and the intersection of the two is empty. The runner
/// printed `nothing selected` and exited zero, which is the shape
/// `tests/inillucent-testing-tdd.md` rule 1.5 is about: a gate that graded
/// nothing read as a pass. Two real names are easier to type than one wrong
/// one, so this was the reachable half of the defect.
///
/// `--changed` is the one case where an empty selection is an answer rather
/// than a failure: it asks what the working tree can break, and "nothing" is
/// both true and useful. So a derived selection stays green and only one the
/// caller named by hand refuses.
///
/// @param options - what the command line asked for
fn empty_selection(options: &Options) -> Result<bool, String> {
    if options.changed.is_some() {
        println!("nothing selected");
        return Ok(true);
    }
    let mut named = Vec::new();
    if !options.targets.is_empty() {
        named.push(format!("--target {}", options.targets.join(", ")));
    }
    if !options.tiers.is_empty() {
        named.push(format!("--tier {}", options.tiers.join(", ")));
    }
    if named.is_empty() {
        return Err(
            "tests/selection.toml names no target, so there was nothing to run".to_string(),
        );
    }
    Err(format!(
        "{} selected no target, so nothing ran. Each name is in tests/selection.toml; \
         no target carries all of them.",
        named.join(" with ")
    ))
}

/// Puts the MSVC compiler's own environment into this process, when it is missing.
///
/// **`onig_sys` compiles oniguruma with `cl.exe`, and an agent terminal has
/// never run `vcvars64.bat` (task-1995, task-2047).** cl.exe finds its headers
/// through INCLUDE, LIB and PATH, so without them the whole run stops on
///
/// ```text
/// regenc.h(39): fatal error C1083: Cannot open include file: 'stddef.h'
/// ```
///
/// which names the header rather than the cause. `Import-MsvcEnvironment` in
/// `packaging/stage-layout.ps1` has done this for the release path since
/// task-1995 and nothing in the test path called it, so every agent running the
/// suite from a terminal that was not a Developer PowerShell got a failed build
/// where a test result should have been. That is one cause of the exit code
/// this ticket is about, and the exit code being right does not make the run
/// have happened.
///
/// Nothing happens when INCLUDE is already set, so a developer shell is left
/// exactly as it is, and nothing happens off an MSVC host.
///
/// A failure here returns the sentence that fixes it instead of letting cargo
/// fail on the header: the build cannot succeed either way, and only one of the
/// two says what to do about it.
#[cfg(all(windows, target_env = "msvc"))]
fn import_msvc_environment() -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    if std::env::var_os("INCLUDE").is_some() {
        return Ok(());
    }
    let vswhere = PathBuf::from(std::env::var("ProgramFiles(x86)").map_err(|_| {
        "INCLUDE is not set and ProgramFiles(x86) is not in this environment, so Visual \
             Studio cannot be found. Run from a Developer PowerShell."
            .to_string()
    })?)
    .join("Microsoft Visual Studio")
    .join("Installer")
    .join("vswhere.exe");
    if !vswhere.is_file() {
        return Err(format!(
            "INCLUDE is not set and {} does not exist, so the MSVC environment cannot be found \
             and `onig_sys` cannot compile oniguruma. Run from a Developer PowerShell, or \
             install Visual Studio's C++ tools.",
            vswhere.display()
        ));
    }
    let found = Command::new(&vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ])
        .output()
        .map_err(|error| format!("cannot run {}: {error}", vswhere.display()))?;
    let install = String::from_utf8_lossy(&found.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if install.is_empty() {
        return Err(
            "INCLUDE is not set and vswhere found no Visual Studio with the C++ tools, so \
             `onig_sys` cannot compile oniguruma. Install Visual Studio's \"Desktop development \
             with C++\" workload."
                .to_string(),
        );
    }
    let vcvars = PathBuf::from(&install)
        .join("VC")
        .join("Auxiliary")
        .join("Build")
        .join("vcvars64.bat");
    if !vcvars.is_file() {
        return Err(format!(
            "INCLUDE is not set and {} does not exist, so the MSVC environment cannot be \
             imported. Run from a Developer PowerShell.",
            vcvars.display()
        ));
    }
    // `set` after the batch file prints the environment it produced, and each
    // line is copied into this process - which is what the cargo children then
    // inherit. `raw_arg` rather than `arg`: cmd.exe does not parse a command
    // line the way Rust quotes one, and the path holds a space.
    let printed = Command::new("cmd")
        .arg("/c")
        .raw_arg(format!("\"\"{}\" >nul 2>&1 && set\"", vcvars.display()))
        .output()
        .map_err(|error| format!("cannot run {}: {error}", vcvars.display()))?;
    if !printed.status.success() {
        return Err(format!("{} did not run.", vcvars.display()));
    }
    for line in String::from_utf8_lossy(&printed.stdout).lines() {
        if let Some((name, value)) = line.split_once('=') {
            if !name.is_empty() {
                std::env::set_var(name, value);
            }
        }
    }
    if std::env::var_os("INCLUDE").is_none() {
        return Err(format!("running {} did not set INCLUDE.", vcvars.display()));
    }
    println!("the MSVC environment from {install}");
    Ok(())
}

/// Nothing to import: the C dependency is not built with cl.exe here.
///
/// `target_env` rather than `windows` alone, so a Windows host on the GNU
/// toolchain is not refused for a compiler it does not use.
#[cfg(not(all(windows, target_env = "msvc")))]
fn import_msvc_environment() -> Result<(), String> {
    Ok(())
}

/// Asks git which paths differ from a revision, including untracked files.
///
/// An untracked file is a new suite or a new crate, and a selector that could
/// not see one would run nothing for exactly the change most likely to need a
/// run.
///
/// @param root - the workspace root
/// @param revision - what to compare against
fn changed_paths(root: &Path, revision: &str) -> Result<Vec<String>, String> {
    let mut paths = BTreeSet::new();
    let tracked = Command::new("git")
        .current_dir(root)
        .args(["diff", "--name-only", revision])
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if !tracked.status.success() {
        return Err(format!(
            "git diff against `{revision}` failed: {}",
            String::from_utf8_lossy(&tracked.stderr).trim()
        ));
    }
    for line in String::from_utf8_lossy(&tracked.stdout).lines() {
        if !line.trim().is_empty() {
            paths.insert(line.trim().to_string());
        }
    }
    let untracked = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    for line in String::from_utf8_lossy(&untracked.stdout).lines() {
        if !line.trim().is_empty() {
            paths.insert(line.trim().to_string());
        }
    }
    Ok(paths.into_iter().collect())
}

/// Builds the selected test targets, and the programs when a selected suite
/// starts one.
///
/// **Only what was selected (task-2114 C3).** This was `cargo test --workspace
/// --no-run --lib --tests` whatever the selection, so a one line change in
/// `inillucent-cli` linked all 227 test binaries and every run compiled
/// `inillucent-bench` with `ort`, `tokenizers` and oniguruma. The arguments now
/// come from `testplan::test_arguments`, which names each selected package and
/// target once. A run that selects no `inillucent-bench` row does not compile
/// it, and so does not need the MSVC environment either.
///
/// `--lib` and named targets and not a bare `--no-run`. A bare one also builds
/// the plain binaries, and one of those is *this program*: cargo cannot replace
/// an executable that is currently running, so the runner's own build step
/// failed with "Access is denied" trying to overwrite itself. `--bin <name>`
/// builds the test harness compiled from that binary's sources, which is where
/// `inillucent-bench` and `inillucent-shell` keep their tests.
///
/// The second half builds the programs the shell suites run, and is the only
/// build of them in a run. The suites that drive a program find it through
/// `cliproc::program`, and `run_one` sets `cliproc::PROGRAMS_BUILT` so that
/// function builds nothing: a build from inside one suite that had to relink a
/// program another suite was running failed on Windows (task-2106).
///
/// **The features ride on the same build (task-1913).** A test behind a feature
/// the build does not turn on is in no binary at all, and `inillucent-core` had
/// twenty-seven such tests that had never run. The rows that name a feature put
/// it in the argument list, so the one build compiles them.
///
/// @param root - the workspace root
/// @param rows - the rows to build, from `testplan::build_set`
fn build(root: &Path, rows: &[&Row]) -> Result<(), String> {
    let arguments = testplan::test_arguments(rows);
    println!(
        "building test targets: cargo test --no-run {}",
        arguments.join(" ")
    );
    let status = Command::new(cargo())
        .current_dir(root)
        .args(["test", "--no-run"])
        .args(&arguments)
        .status()
        .map_err(|error| format!("cannot run cargo: {error}"))?;
    if !status.success() {
        return Err("the build failed".to_string());
    }
    if !testplan::needs_programs(rows) {
        println!("no selected suite starts a program, so inillucent-cli is not built");
        return Ok(());
    }
    let status = Command::new(cargo())
        .current_dir(root)
        .args([
            "build",
            "-p",
            "inillucent-cli",
            "-p",
            "inillucent-driver-capi",
        ])
        .status()
        .map_err(|error| format!("cannot run cargo: {error}"))?;
    if !status.success() {
        return Err("building the shell and the C ABI failed".to_string());
    }
    Ok(())
}

/// Returns the cargo to invoke.
fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

/// Asks cargo where the executables it just built are.
///
/// The answer comes from `--message-format=json`, read with the engine's own
/// JSON parser. Guessing at `target/debug/deps/<name>-<hash>` would be guessing
/// at a hash and at which of several stale copies is the current one.
///
/// **The same arguments the build used, or this finds the wrong binaries.** A
/// different feature set is a different compilation with a different hash, so
/// asking without them returns the default executables, which is how a run
/// could build the feature tests and then not run them (task-1913). A wider
/// target set would build what the narrowed build left out.
///
/// Every executable cargo reports comes back, not only the rows asked for:
/// `--lib` builds the library harness of every named package, and the artifact
/// list a nested run reads should hold everything that exists.
///
/// @param root - the workspace root
/// @param rows - the rows that were built
fn locate(root: &Path, rows: &[&Row]) -> Result<Vec<Artifact>, String> {
    let output = Command::new(cargo())
        .current_dir(root)
        .args(["test", "--no-run"])
        .args(testplan::test_arguments(rows))
        .arg("--message-format=json")
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("cannot run cargo: {error}"))?;
    if !output.status.success() {
        return Err("cargo could not list the built targets".to_string());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut found: BTreeMap<Target, Artifact> = BTreeMap::new();
    for line in text.lines() {
        if let Some(artifact) = read_artifact(line) {
            found.insert(artifact.target.clone(), artifact);
        }
    }
    Ok(found.into_values().collect())
}

/// Returns the executable for each selected row, or names the rows with none.
///
/// @param artifacts - what was built, from `locate` or from an artifact list
/// @param rows - the rows the run will start
fn pick(artifacts: &[Artifact], rows: &[&Row]) -> Result<Vec<Built>, String> {
    let mut built = Vec::new();
    let mut missing = Vec::new();
    for row in rows {
        match artifacts
            .iter()
            .find(|artifact| artifact.target == row.target.binary())
        {
            Some(artifact) => built.push(Built {
                target: row.target.clone(),
                executable: artifact.executable.clone(),
                directory: artifact.directory.clone(),
                tests: None,
            }),
            None => missing.push(row.target.label()),
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "cargo built no executable for: {}",
            missing.join(", ")
        ));
    }
    Ok(built)
}

/// Writes the artifact list into the target directory and returns its path.
///
/// The target directory comes from `cargo metadata`, because a worktree's
/// `.cargo/config.toml` moves it off the checkout and onto another drive.
///
/// @param root - the workspace root
/// @param artifacts - what `locate` found
fn write_artifact_list(root: &Path, artifacts: &[Artifact]) -> Result<PathBuf, String> {
    let output = Command::new(cargo())
        .current_dir(root)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("cannot run cargo: {error}"))?;
    if !output.status.success() {
        return Err("cargo metadata did not say where the target directory is".to_string());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let directory = parse::parse(&text)
        .ok()
        .and_then(|parsed| {
            testplan::json_field(&parsed.node, "target_directory").and_then(testplan::json_text)
        })
        .ok_or("cargo metadata named no target directory")?;
    let path = testplan::artifacts_path(Path::new(&directory));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    std::fs::write(&path, testplan::render_artifacts(artifacts))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(path)
}

/// Reads an artifact list another run wrote.
///
/// A list that is not there is a run that cannot happen, so it is an error
/// naming the path, which `main` turns into exit code 2.
///
/// @param file - the list `--artifacts` named
fn read_artifact_list(file: &Path) -> Result<Vec<Artifact>, String> {
    let text = std::fs::read_to_string(file).map_err(|error| {
        format!(
            "the artifact list {} cannot be read ({error}), so there is nothing to run",
            file.display()
        )
    })?;
    testplan::parse_artifacts(&text).map_err(|reason| {
        format!(
            "the artifact list {} is not readable: {reason}",
            file.display()
        )
    })
}

/// Reads one `compiler-artifact` line into the target, its executable and the
/// directory cargo would run it from.
///
/// Two filters matter here and both are load-bearing.
///
/// `profile.test` must be true. Cargo emits **two** artifacts for every binary
/// under `--no-run`: the program itself, and the test harness compiled from the
/// same sources. They have the same kind and the same name, and only the
/// profile tells them apart - so a reader that took the first would run
/// `inillucent-shell` as a program, get a shell waiting on standard input, and
/// hang the run.
///
/// `executable` must be present. A library's `rlib` is an artifact with no
/// executable, and its test harness is a second artifact that has one.
///
/// @param line - one line of cargo's JSON stream
fn read_artifact(line: &str) -> Option<Artifact> {
    let parsed = parse::parse(line).ok()?;
    let node = parsed.node;
    if json_text(json_field(&node, "reason")?)? != "compiler-artifact" {
        return None;
    }
    if !matches!(
        json_field(json_field(&node, "profile")?, "test")?,
        Node::True
    ) {
        return None;
    }
    let executable = json_text(json_field(&node, "executable")?)?;
    let manifest = json_text(json_field(&node, "manifest_path")?)?;
    let target = json_field(&node, "target")?;
    let name = json_text(json_field(target, "name")?)?;
    let kind = match json_field(target, "kind")? {
        Node::Array(items) => json_text(items.first()?)?,
        other => json_text(other)?,
    };
    let manifest = PathBuf::from(manifest);
    let directory = manifest.parent()?.to_path_buf();
    let package = read_package_name(&manifest)?;
    let kind = match kind.as_str() {
        "lib" => Kind::Lib,
        "test" => Kind::Test,
        "bin" => Kind::Bin,
        // `proc-macro`, `custom-build` and the rest are not things this
        // workspace has, and running one would not mean anything if it did.
        _ => return None,
    };
    let name = match kind {
        Kind::Lib => "lib".to_string(),
        Kind::Test | Kind::Bin => name,
    };
    Some(Artifact {
        target: Target {
            package,
            kind,
            name,
            module: None,
        },
        executable: PathBuf::from(executable),
        directory,
    })
}

/// Reads the `name` out of a manifest's `[package]` section.
///
/// @param path - the manifest cargo named
fn read_package_name(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_package = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let (key, body) = line.split_once('=')?;
        if key.trim() == "name" {
            return Some(body.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// Reads the recorded times, ignoring a ledger that is absent or unreadable.
///
/// A missing ledger must not stop a run: it only costs a worse order, and the
/// first `--record` writes one.
///
/// @param path - the ledger
fn read_ledger(path: &Path) -> BTreeMap<String, u64> {
    let mut times = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return times;
    };
    let Ok(document) = inillucent_compat::toml_lite::parse(&text) else {
        return times;
    };
    for row in document.array("timing") {
        let Some(target) = row
            .get("target")
            .and_then(inillucent_compat::toml_lite::Value::as_str)
        else {
            continue;
        };
        let Some(milliseconds) = row
            .get("milliseconds")
            .and_then(inillucent_compat::toml_lite::Value::as_integer)
        else {
            continue;
        };
        if milliseconds >= 0 {
            times.insert(target.to_string(), milliseconds as u64);
        }
    }
    times
}

/// Writes the ledger back, keeping times for targets this run did not touch.
///
/// @param path - the ledger
/// @param previous - what it held before
/// @param outcomes - what this run measured
fn write_ledger(
    path: &Path,
    previous: &BTreeMap<String, u64>,
    outcomes: &[Outcome],
) -> Result<(), String> {
    let mut times = previous.clone();
    for outcome in outcomes {
        // A failure's duration is not a measurement of the suite: it is a
        // measurement of how long it took to hit the first assertion, which is
        // usually much shorter and would poison the schedule.
        if outcome.verdict.is_green() {
            let milliseconds = u64::try_from(outcome.elapsed.as_millis()).unwrap_or(u64::MAX);
            times.insert(outcome.target.label(), milliseconds);
        }
    }
    let mut text = String::from(
        "# Measured wall-clock per test target, in milliseconds.\n\
         #\n\
         # Written by `inillucent-testrun --record`, and read back to schedule the\n\
         # longest targets first. It is committed because the schedule has to be\n\
         # good on the first run on a fresh clone, not on the second.\n\
         #\n\
         # These are times under the parallel runner on a 24-core machine, so they\n\
         # are longer than the same suite's time to itself. That is the right\n\
         # number for ordering: it is what the suite costs the run it is part of.\n\n",
    );
    for (target, milliseconds) in &times {
        text.push_str("[[timing]]\n");
        text.push_str(&format!("target = \"{target}\"\n"));
        text.push_str(&format!("milliseconds = {milliseconds}\n\n"));
    }
    std::fs::write(path, text).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// Returns how long each target may run before the runner stops waiting for it.
///
/// Drawn from the same ledger the schedule is drawn from, so the number that
/// says a target is slow and the number that says it is stuck are the same
/// measurement. A target the ledger has never seen gets the floor rather than a
/// multiple of a guess - `supervise::budget` is where that arithmetic and its
/// tests live.
///
/// **A budget is not what stops a target on its own.** It is one of the two
/// conditions `supervise` requires; the other is that the target has printed
/// nothing for ten minutes. A suite that replays its work through the pinned
/// SQLite shell sits at flat processor time for as long as the shell takes, and
/// a bound that fired on that would kill working suites - which is a mistake
/// that was made three times by hand on this machine in one evening, and cost a
/// legitimate thirty minute run once.
///
/// @param targets - every target this run will start
/// @param ledger - the recorded times
/// @param options - what the command line asked for
fn budgets(
    targets: &BTreeMap<Target, Built>,
    ledger: &BTreeMap<String, u64>,
    options: &Options,
) -> BTreeMap<Target, Option<Duration>> {
    targets
        .keys()
        .map(|target| {
            let limit = match options.timeout {
                Some(0) => None,
                Some(seconds) => Some(Duration::from_secs(seconds)),
                None => Some(supervise::budget(
                    ledger
                        .get(&target.label())
                        .copied()
                        .map(Duration::from_millis),
                )),
            };
            (target.clone(), limit)
        })
        .collect()
}

/// Puts the longest targets first.
///
/// Longest-processing-time-first is the classic answer to this shape of
/// problem, and the reason is the tail: whatever starts last decides when the
/// run ends, so the worst thing to start last is the slowest suite. On this
/// workspace the difference is not marginal - a handful of campaigns take
/// minutes while most suites take under a second.
///
/// @param built - the located executables
/// @param ledger - the recorded times
fn schedule(mut built: Vec<Built>, ledger: &BTreeMap<String, u64>) -> Vec<Built> {
    built.sort_by(|left, right| {
        let left_time = ledger
            .get(&left.target.label())
            .copied()
            .unwrap_or(UNKNOWN_MILLISECONDS);
        let right_time = ledger
            .get(&right.target.label())
            .copied()
            .unwrap_or(UNKNOWN_MILLISECONDS);
        right_time
            .cmp(&left_time)
            .then_with(|| left.target.cmp(&right.target))
    });
    built
}

/// Runs the binaries, at most `jobs` at a time, reporting each as it finishes.
///
/// @param ordered - the targets, longest first
/// @param options - what the command line asked for
/// @param budgets - how long each target may run
fn execute(
    ordered: Vec<Built>,
    options: &Options,
    budgets: &BTreeMap<Target, Option<Duration>>,
) -> Vec<Outcome> {
    let total = ordered.len();
    let (sender, receiver) = mpsc::channel::<Outcome>();
    let mut pending = ordered.into_iter();
    let mut running = 0usize;
    let mut outcomes = Vec::with_capacity(total);
    let mut finished = 0usize;

    let start_one = |pending: &mut std::vec::IntoIter<Built>| -> bool {
        let Some(built) = pending.next() else {
            return false;
        };
        let sender = sender.clone();
        let threads = options.test_threads.to_string();
        let filter = options.filter.clone();
        let strict = options.strict;
        let budget = budgets.get(&built.target).copied().flatten();
        let _ = std::thread::Builder::new()
            .name(built.target.label())
            .spawn(move || {
                let outcome = run_one(&built, &threads, filter.as_deref(), strict, budget);
                let _ = sender.send(outcome);
            });
        true
    };

    while running < options.jobs && start_one(&mut pending) {
        running += 1;
    }
    while running > 0 {
        let Ok(outcome) = receiver.recv() else {
            break;
        };
        running -= 1;
        finished += 1;
        println!(
            "  [{finished:>3}/{total}] {:<7} {:<44} {:>7.2}s  {} test(s)",
            outcome.verdict.word(),
            outcome.target.label(),
            outcome.elapsed.as_secs_f64(),
            outcome.ran
        );
        let _ = std::io::stdout().flush();
        outcomes.push(outcome);
        if start_one(&mut pending) {
            running += 1;
        }
    }
    outcomes
}

/// Runs one target and reads its result.
///
/// A whole binary runs once, with the `--filter` text if there is one. A module
/// target runs its binary with `--exact` and the module's own test names,
/// which `module_tests` already narrowed by `--filter`. A list too long for
/// one command line runs as several processes of the same binary one after the
/// other, and their outcomes are added into one: the target is still one row,
/// one timing and one verdict.
///
/// @param built - the executable and where to run it
/// @param threads - what to pass as `--test-threads`
/// @param filter - a name filter, when one was asked for
/// @param strict - whether a suite that skips should panic rather than pass
/// @param budget - how long it may run before it is stopped; `None` never stops it
fn run_one(
    built: &Built,
    threads: &str,
    filter: Option<&str>,
    strict: bool,
    budget: Option<Duration>,
) -> Outcome {
    let Some(names) = &built.tests else {
        let words: Vec<String> = filter.map(str::to_string).into_iter().collect();
        return run_process(built, threads, &words, false, strict, budget);
    };
    let mut merged: Option<Outcome> = None;
    for chunk in testplan::exact_chunks(names, testplan::EXACT_LIST_LIMIT) {
        let outcome = run_process(built, threads, &chunk, true, strict, budget);
        merged = Some(match merged {
            None => outcome,
            Some(earlier) => merge_outcomes(earlier, outcome),
        });
    }
    merged.unwrap_or_else(|| Outcome {
        target: built.target.clone(),
        verdict: Verdict::Undetermined(Undetermined::NeverStarted),
        elapsed: Duration::ZERO,
        ran: 0,
        output: "no test list to run".to_string(),
        status: "never started".to_string(),
        retry_of: None,
    })
}

/// Adds a second process's outcome for the same target to the first.
///
/// The worse verdict wins: a target that could not be read in one part could
/// not be read, and one that failed in one part failed.
///
/// @param earlier - the outcome so far
/// @param later - the next part's outcome
fn merge_outcomes(earlier: Outcome, later: Outcome) -> Outcome {
    let verdict = match (earlier.verdict, later.verdict) {
        (Verdict::Undetermined(reason), _) | (_, Verdict::Undetermined(reason)) => {
            Verdict::Undetermined(reason)
        }
        (Verdict::Failed, _) | (_, Verdict::Failed) => Verdict::Failed,
        _ => Verdict::Passed,
    };
    Outcome {
        target: earlier.target,
        verdict,
        elapsed: earlier.elapsed + later.elapsed,
        ran: earlier.ran + later.ran,
        output: format!("{}\n{}", earlier.output, later.output),
        status: format!("{}; {}", earlier.status, later.status),
        retry_of: earlier.retry_of,
    }
}

/// Runs one process of a test binary and reads its result.
///
/// @param built - the executable and where to run it
/// @param threads - what to pass as `--test-threads`
/// @param words - a filter, or the exact test names
/// @param exact - whether `words` are exact names
/// @param strict - whether a suite that skips should panic rather than pass
/// @param budget - how long it may run before it is stopped; `None` never stops it
fn run_process(
    built: &Built,
    threads: &str,
    words: &[String],
    exact: bool,
    strict: bool,
    budget: Option<Duration>,
) -> Outcome {
    let mut command = Command::new(&built.executable);
    command
        .current_dir(&built.directory)
        // `conformance.rs` in `inillucent-driver-capi` reads `CARGO` to find
        // the cargo that is driving it. Started from here rather than from cargo, they would
        // fall back to whatever `cargo` is on PATH, which on a machine with
        // several toolchains is not necessarily this one.
        .env("CARGO", cargo())
        // **The suites use the programs `build` made, and build nothing
        // themselves (task-2106).** `cliproc::program` used to run `cargo build
        // -p inillucent-cli` from inside every suite, and a build that had to
        // relink while another suite was running `inillucent-shell.exe` failed
        // with `Access is denied. (os error 5)` and was reported as a missing
        // prerequisite.
        .env(inillucent_compat::cliproc::PROGRAMS_BUILT, "1")
        // **What `--strict` means, handed to the suite itself.** The
        // classifier below reads a suite's captured output and decides whether
        // it skipped, which works and is the backstop; this is the same
        // decision made one layer earlier, where the *test* is still on the
        // stack. A suite that skips under `--strict` panics with the name of
        // the test and the name of the thing that is missing, rather than
        // passing and being classified afterwards by binary. `INILLUCENT_STRICT`
        // is unset for an ordinary run, so a developer without the oracle still
        // gets a green suite that says what it skipped (task-1932, H10).
        .env("INILLUCENT_STRICT", if strict { "1" } else { "" })
        // **`--show-output`, or `--strict` cannot see a skip at all.**
        // libtest swallows the output of a test that *passes*, and
        // a suite whose prerequisite is absent passes - that is the whole shape
        // of the problem. So the sentence a skipping suite prints to say what is
        // missing never reached this process, `missing_prerequisites`'s
        // `announced` branch could never fire, and the only skip `--strict`
        // could ever catch was a suite that ran literally zero tests. A run on a
        // machine without the pinned oracle therefore reported `ok` with thirty
        // differential suites having asserted nothing.
        //
        // **`--show-output` rather than `--nocapture`**, which was tried first
        // and reverted. `--nocapture` turns the capture *off*, so every test
        // writes straight to the pipe with no serialisation between the two
        // threads each binary runs - and `inillucent-bench`, which loads the
        // ONNX runtime and its CUDA provider, then died at teardown with
        // `0xC0000409` in two of three full runs while passing every one of its
        // 156 tests. It had not failed once in four full runs without the flag,
        // and passed 3/3 run on its own with it, so the crash needed the flag
        // *and* the load of a full parallel run. `--show-output` keeps the
        // capture and prints a passing test's output in the summary instead,
        // which is all this needs.
        //
        // It costs a noisier transcript, which nobody sees: the output is kept
        // in the `Outcome` and printed only for a failure.
        .args(["--test-threads", threads, "--show-output"]);
    if exact {
        command.arg("--exact");
    }
    command.args(words);
    let limits = Limits {
        budget,
        ..Limits::default()
    };
    // **Not `Command::output()`, and that is the whole of task-2071.**
    // `output()` waits for the child's pipes to reach end of file, which is a
    // different event from the child exiting: anything the child started with
    // inherited standard output holds the write end after the child is gone, so
    // `output()` goes on waiting with no child of its own and no verdict to
    // report. The runner did exactly that - alive at 1.625 seconds of processor
    // time, no children, no result line, no summary and no exit code - and had
    // to be stopped by its process id. `supervise` waits on the child.
    let started = Instant::now();
    match supervise::supervise(&mut command, &limits) {
        Ok(run) => read_run(built, run),
        Err(error) => Outcome {
            target: built.target.clone(),
            verdict: Verdict::Undetermined(Undetermined::NeverStarted),
            elapsed: started.elapsed(),
            ran: 0,
            output: format!("cannot start {}: {error}", built.executable.display()),
            status: "never started".to_string(),
            retry_of: None,
        },
    }
}

/// Turns what the supervisor saw into the outcome the report prints.
///
/// @param built - which target ran
/// @param run - what the supervisor produced
fn read_run(built: &Built, run: supervise::Supervised) -> Outcome {
    let mut text = String::from_utf8_lossy(&run.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&run.stderr));
    let summary = verdict::read_summary(&text).unwrap_or_default();
    let ran = summary.passed + summary.failed;
    if let Stopped::Killed {
        ran_for,
        silent_for,
    } = run.stopped
    {
        // Undetermined rather than FAILED, and the distinction is not cosmetic.
        // Nothing was graded here: the tests this target got through before it
        // was stopped are not evidence that the rest would have passed, and they
        // are not evidence that any of them would have failed either. The run
        // goes red and the target is named, which is what was missing.
        return Outcome {
            target: built.target.clone(),
            verdict: Verdict::Undetermined(Undetermined::TimedOut),
            elapsed: run.elapsed,
            ran,
            output: text,
            status: format!(
                "killed after {:.1}s, having printed nothing for the last {:.1}s of it",
                ran_for.as_secs_f64(),
                silent_for.as_secs_f64()
            ),
            retry_of: None,
        };
    }
    let mut status = match run.status.and_then(|status| status.code()) {
        Some(code) => format!("exit status {code}"),
        None => "killed by a signal".to_string(),
    };
    if run.stopped == Stopped::ExitedHoldingPipes {
        // Said out loud, because it is the one case where the transcript below
        // may be short of what the target actually printed - and because it
        // names a real thing about the suite: it left a process running.
        status.push_str(" (something it started outlived it holding its output pipe)");
    }
    Outcome {
        target: built.target.clone(),
        verdict: verdict::classify(&text, run.status.is_some_and(|status| status.success())),
        elapsed: run.elapsed,
        ran,
        output: text,
        status,
        retry_of: None,
    }
}

/// Runs each target the first pass could not read once more, on its own.
///
/// **Why this is not a retry of failures.** A runner that re-runs a failing test
/// until it passes has stopped being a gate. This re-runs only a target whose
/// first attempt produced no readable answer - the harness said everything passed
/// and the process then died, or there was no summary to read at all - which is
/// an absence of information rather than a result. One more attempt, alone,
/// replaces a guess about which way to round the first one with a second
/// measurement.
///
/// Whatever the second attempt says stands, and the first attempt's reason and
/// exit status travel with it into the report either way.
///
/// A target the runner **stopped** is not retried either, and for the opposite
/// reason. Its first attempt was not an absence of information: the runner
/// refused to wait any longer, and a second attempt would refuse again at the
/// same budget, having spent it twice. What there is to say about a stopped
/// target is in the report.
///
/// @param outcomes - what the first pass produced
/// @param executables - the built binaries, by target
/// @param options - what the command line asked for
/// @param budgets - how long each target may run
fn settle_undetermined(
    outcomes: Vec<Outcome>,
    executables: &BTreeMap<Target, Built>,
    options: &Options,
    budgets: &BTreeMap<Target, Option<Duration>>,
) -> Vec<Outcome> {
    let unread: Vec<(Target, Undetermined, String)> = outcomes
        .iter()
        .filter_map(|outcome| {
            outcome
                .verdict
                .undetermined()
                .filter(|reason| *reason != Undetermined::TimedOut)
                .map(|reason| (outcome.target.clone(), reason, outcome.status.clone()))
        })
        .collect();
    if unread.is_empty() {
        return outcomes;
    }
    let mut settled = outcomes;
    println!(
        "\n{} target(s) could not be read; running each one alone, once more",
        unread.len()
    );
    let threads = options.test_threads.to_string();
    for (target, reason, status) in unread {
        let Some(built) = executables.get(&target) else {
            continue;
        };
        let budget = budgets.get(&target).copied().flatten();
        let mut again = run_one(
            built,
            &threads,
            options.filter.as_deref(),
            options.strict,
            budget,
        );
        again.retry_of = Some(format!(
            "{} on the first attempt ({status})",
            reason.reason()
        ));
        println!(
            "  {:<7} {:<44} {:>7.2}s  {} test(s)  [second attempt]",
            again.verdict.word(),
            target.label(),
            again.elapsed.as_secs_f64(),
            again.ran
        );
        let _ = std::io::stdout().flush();
        if let Some(slot) = settled.iter_mut().find(|outcome| outcome.target == target) {
            *slot = again;
        }
    }
    settled
}

/// Returns how many tests a transcript says failed.
///
/// Read out of libtest's own summary line rather than counted from the failure
/// list, because the list is printed twice - once as it happens and once in the
/// summary - and counting it would double every number.
///
/// @param output - everything the suite printed
fn failed_count(output: &str) -> usize {
    output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("test result: "))
        .filter_map(|rest| rest.split(';').nth(1))
        .filter_map(|part| part.trim().strip_suffix(" failed"))
        .filter_map(|count| count.parse::<usize>().ok())
        .sum()
}

/// Returns what a strict run's skip panics said was missing, de-duplicated.
///
/// The suite's own sentence, which is better evidence than a `requires` row: it
/// names the thing this machine has not got rather than the category the map
/// put the target in.
///
/// @param output - everything the suite printed
fn strict_skip_reasons(output: &str) -> Vec<String> {
    let mut said: Vec<String> = Vec::new();
    for line in output.lines() {
        let Some(at) = line.find("; skipping") else {
            continue;
        };
        if !line.contains(inillucent_compat::differential::STRICT_SKIP) {
            continue;
        }
        // The panic prints `<file>:<line>:<column>:` ahead of the message on
        // the line libtest captures, and the reason is what is between that and
        // the marker.
        let head = line.get(..at).unwrap_or_default();
        let reason = head.rsplit(": ").next().unwrap_or(head).trim().to_string();
        if !reason.is_empty() && !said.contains(&reason) {
            said.push(reason);
        }
    }
    said
}

/// Returns the selected suites whose prerequisites were not there.
///
/// The harness says so itself: every one of these suites prints a line naming
/// what is missing rather than failing, so the runner reads the transcript
/// instead of trying to detect the prerequisite for itself. Detecting it
/// separately would be a second opinion that could disagree with the suite's.
///
/// @param outcomes - what ran
/// @param map - the selection map, for what each target requires
fn missing_prerequisites<'run>(
    outcomes: &'run [Outcome],
    map: &Map,
) -> Vec<(&'run Outcome, Vec<String>)> {
    let mut hollow = Vec::new();
    for outcome in outcomes {
        // **A target that failed is hollow only when every one of its failures
        // is a strict skip (task-1932, H10).** The rule used to be that only a
        // passing target could be hollow, which was right while a skip was a
        // `return` - it evidenced a problem, and a second label would have been
        // wrong. `--strict` now makes a skip panic, so the same suite fails
        // instead, and calling that a failure is the wrong label the other way
        // round: it did not evidence a problem, it evidenced nothing. The
        // sentinel `differential::skipping` panics with is what separates the
        // two, and a target with even one failure that does not carry it stays a
        // failure.
        let strictly_skipped = inillucent_compat::differential::every_failure_is_a_strict_skip(
            &outcome.output,
            failed_count(&outcome.output),
        );
        if !outcome.verdict.is_green() && !strictly_skipped {
            continue;
        }
        let Some(row) = map.row(&outcome.target) else {
            continue;
        };
        // **A suite that said what it was missing does not need a `requires`
        // row to be believed (task-1932, H10).** The rule was that a target
        // with no declared prerequisite could not be hollow, which was safe
        // while a skip was a silent `return`: there was nothing to read. Under
        // `--strict` the suite panics with the reason, and dropping it here
        // because `selection.toml` happens not to declare one would put the
        // skip back where it started - invisible, with the run reporting that
        // every test passed while two of them did not run.
        //
        // `confinement` is the case that showed it: it needs a platform that
        // can make a directory link, which its row does not say and which a
        // Windows session without developer mode cannot do.
        let strict_reasons = strict_skip_reasons(&outcome.output);
        if row.requires.is_empty() && strict_reasons.is_empty() {
            continue;
        }
        // A suite that ran no tests at all, or that said out loud that its
        // reference is absent, evidenced nothing.
        //
        // **The phrase list used to miss almost every suite it was written
        // for.** It matched `has not been built`,
        // `is not available` and `no reference`; what the suites actually
        // print is `the pinned SQLite oracle is not built; skipping`,
        // `the pinned shell is not present; skipping` and `no usable C
        // compiler; skipping` - not one of which matched. §9 of
        // `tests/inillucent-testing-tdd.md` had meanwhile told authors to print
        // `is not built`, `is missing` or `; skipping`, so the standard and the
        // code had been describing two different lists. The consequence was
        // exactly what `--strict` exists to prevent: on a machine without the
        // oracle, thirty-odd differential suites skipped every case and the run
        // reported `ok`.
        //
        // **One phrase as of task-1932, and every site was moved onto it.**
        // The other five substrings are gone rather than kept "in case", which
        // is what let two phrasings exist that matched none of them: a list of
        // near-misses is a list nobody checks against, and
        // `policy::every_skip_site_carries_the_one_marker` now greps every
        // `eprintln!` that precedes an early return and names any that does not
        // end with the marker. A message written to the old list fails that
        // check rather than being quietly half-recognised here.
        let silent = outcome.ran == 0;
        let announced = inillucent_compat::differential::announces_a_skip(&outcome.output);
        if silent || announced {
            // **What the suite said comes first, and the row is the fallback
            // (task-2101).** The row lists everything a suite *could* be
            // missing, and the suite names what it *was* missing. With the row
            // first, `conformance` failing to relink a DLL that Python had
            // loaded was printed as `needs cc, asan`, which sent the reader
            // looking for a missing toolchain when the suite itself had said
            // "the C ABI static library did not build". A non-strict run has no
            // panic to read the reason from, so it still prints the row.
            let said = if strict_reasons.is_empty() {
                row.requires.clone()
            } else {
                strict_reasons
            };
            hollow.push((outcome, said));
        }
    }
    hollow
}

/// Prints what the selected suites needed, and returns how many went without.
///
/// **What a fully provisioned machine gets to say (task-1969, 9).** Until the
/// first of these two lines, a run where every prerequisite was present printed
/// nothing about prerequisites at all - identical output to a run on a
/// workspace that declares none. So the reader of a green run could not tell
/// "the oracle, the shell and the fixtures were all here and forty suites used
/// them" from "nothing here needs anything", and the second is what the page
/// used to imply.
///
/// It counts rows rather than suites that skipped, because that is the
/// question: of the targets this selection included that declare a
/// prerequisite, how many ran with it present.
///
/// **A declared absence gets its own heading (task-2114 C8).** A target whose
/// every missing thing is in `tests/prerequisites.local.toml` is listed under
/// "not evidenced on this machine, by declaration", with the declared list, and
/// does not fail a strict run. The list is printed so a reader of a green run
/// can see what it did not evidence, and a release note can say so.
///
/// @param strict - whether a missing prerequisite is a failure
/// @param absences - the hollow targets, split by the declaration
fn report_prerequisites(strict: bool, absences: &Absences) -> usize {
    let hollow = &absences.unexcused;
    let declared = absences.declaring;
    if declared > 0 {
        println!();
        println!(
            "{} of {declared} selected suite(s) that declare a prerequisite had it",
            declared.saturating_sub(hollow.len() + absences.excused.len())
        );
    }
    if !absences.excused.is_empty() {
        println!(
            "{} suite(s) not evidenced on this machine, by declaration ({} declares absent: {}):",
            absences.excused.len(),
            testplan::DECLARED_ABSENCES_FILE,
            absences.declared.join(", ")
        );
        for (outcome, said) in &absences.excused {
            println!("  {:<44} {}", outcome.target.label(), said.join(", "));
        }
    }
    if hollow.is_empty() {
        return 0;
    }
    println!(
        "{} suite(s) ran without a prerequisite and evidenced nothing{}:",
        hollow.len(),
        if strict {
            ""
        } else {
            " (--strict makes this a failure)"
        }
    );
    for (outcome, requires) in hollow {
        // **"needs" only in front of a `requires` row.** Those are names of
        // things - `postgres`, `oracle`, `shell` - and read as a need. A reason
        // the suite printed is a whole sentence about this machine, and putting
        // "needs" in front of one produces "needs this platform would not make a
        // directory link" (task-1932, H10).
        let said = requires.join(", ");
        let lead = if said.split_whitespace().count() > 3 {
            ""
        } else {
            "needs "
        };
        println!("  {:<44} {lead}{}", outcome.target.label(), said);
    }
    hollow.len()
}

/// Prints the summary, and every failure in full.
///
/// @param outcomes - what ran
/// @param wall - how long the whole run took
/// @param strict - whether a missing prerequisite is a failure
/// @param absences - the hollow targets, split by the declaration
fn report(outcomes: &[Outcome], wall: Duration, strict: bool, absences: &Absences) {
    // **A suite whose every failure is a strict skip is not a failure
    // (task-1932, H10).** It is listed under "evidenced nothing" below, with
    // what it was missing, because that is what it did: `--strict` turns a skip
    // into a failed *test* so the case is named rather than the binary, and
    // reporting the binary as FAILED afterwards would say a defect was found
    // when none was. The exit status is unchanged - a strict run still exits 1
    // for it - which is the whole point of `--strict`.
    let failures: Vec<&Outcome> = outcomes
        .iter()
        .filter(|outcome| matches!(outcome.verdict, Verdict::Failed))
        .filter(|outcome| {
            !inillucent_compat::differential::every_failure_is_a_strict_skip(
                &outcome.output,
                failed_count(&outcome.output),
            )
        })
        .collect();
    let unread: Vec<&Outcome> = outcomes
        .iter()
        .filter(|outcome| outcome.verdict.undetermined().is_some())
        .collect();
    // Listed apart from the rest of `unread` below, because the sentence that
    // list ends on - that a second attempt did not settle it either - is not
    // true of these and must not be printed over them. A stopped target was
    // never retried, on purpose.
    let stopped: Vec<&Outcome> = outcomes
        .iter()
        .filter(|outcome| outcome.verdict.undetermined() == Some(Undetermined::TimedOut))
        .collect();
    // The transcript of a target that could not be read is printed for the same
    // reason a failure's is: it is the only account of what happened, and the
    // summary line inside it is what says the tests themselves passed.
    for outcome in failures.iter().chain(unread.iter()) {
        match outcome.verdict.undetermined() {
            Some(reason) => println!(
                "\n=== {} (UNKNOWN, {}: {}) ===",
                outcome.target.label(),
                outcome.status,
                reason.reason()
            ),
            None => println!("\n=== {} ({}) ===", outcome.target.label(), outcome.status),
        }
        if let Some(first) = &outcome.retry_of {
            println!("(second attempt; {first})");
        }
        println!("{}", outcome.output.trim_end());
    }

    let tests: usize = outcomes.iter().map(|outcome| outcome.ran).sum();
    let serial: Duration = outcomes.iter().map(|outcome| outcome.elapsed).sum();
    println!("\n--- summary ---");
    println!(
        "{} target(s), {} test(s), {} failed, {} undetermined",
        outcomes.len(),
        tests,
        failures.len(),
        unread.len()
    );
    println!(
        "wall {:.1}s; the same work run one at a time is {:.1}s of processor time ({:.1}x)",
        wall.as_secs_f64(),
        serial.as_secs_f64(),
        if wall.as_secs_f64() > 0.0 {
            serial.as_secs_f64() / wall.as_secs_f64()
        } else {
            0.0
        }
    );

    let mut slowest: Vec<&Outcome> = outcomes.iter().collect();
    slowest.sort_by_key(|outcome| std::cmp::Reverse(outcome.elapsed));
    println!("slowest:");
    for outcome in slowest.iter().take(8) {
        println!(
            "  {:>7.2}s  {}",
            outcome.elapsed.as_secs_f64(),
            outcome.target.label()
        );
    }

    let hollow = report_prerequisites(strict, absences);

    let settled: Vec<&Outcome> = outcomes
        .iter()
        .filter(|outcome| outcome.retry_of.is_some() && outcome.verdict.is_green())
        .collect();
    if !settled.is_empty() {
        println!(
            "\n{} target(s) passed on a second attempt, having been unreadable on the first:",
            settled.len()
        );
        for outcome in &settled {
            println!(
                "  {:<44} {}",
                outcome.target.label(),
                outcome.retry_of.as_deref().unwrap_or("")
            );
        }
    }

    if failures.is_empty() && unread.is_empty() {
        // **`ok` is what a reader remembers, so it is not printed on a run that
        // is about to exit 1.** Under `--strict` a suite whose prerequisite is
        // absent fails the run, and the summary said `ok` over the top of it.
        // That is the same defect as the skip phrases that let a green with
        // nothing installed pass for evidence, and as a target recorded FAILED
        // with all 156 of its tests passing: the report and the exit status
        // have to agree, or one of them stops being read.
        // A run where every binary started and counted no test is the same
        // defect reached through `--filter`, and `run` exits 2 for it. The word
        // here has to agree with that code for the reason above.
        if nothing_was_graded(outcomes) {
            println!("\nnot ok - every target ran and no test was graded");
            return;
        }
        if strict && hollow > 0 {
            println!("\nnot ok - every test passed, and {hollow} suite(s) evidenced nothing");
            return;
        }
        println!("\nok");
        return;
    }
    if !failures.is_empty() {
        println!("\nFAILED:");
        for failure in &failures {
            // **With the exit status, which is the one fact that separates the
            // two things this list is used to mean.** A target here either
            // failed a test or died after its tests passed, and a reader given
            // only the name cannot tell which - which is exactly the question
            // somebody had to answer by hand when `inillucent-bench` printed
            // `156 passed; 0 failed` and the run recorded it FAILED.
            println!("  {:<44} {}", failure.target.label(), failure.status);
        }
    }
    if !stopped.is_empty() {
        println!("\nSTOPPED - the runner would not wait any longer, so nothing here was graded:");
        for outcome in &stopped {
            println!("  {:<44} {}", outcome.target.label(), outcome.status);
        }
    }
    let unsettled: Vec<&&Outcome> = unread
        .iter()
        .filter(|outcome| outcome.verdict.undetermined() != Some(Undetermined::TimedOut))
        .collect();
    if !unsettled.is_empty() {
        println!("\nUNDETERMINED - a second attempt alone did not settle these either:");
        for outcome in &unsettled {
            println!(
                "  {:<44} {}, {}",
                outcome.target.label(),
                outcome.status,
                outcome
                    .verdict
                    .undetermined()
                    .map(Undetermined::reason)
                    .unwrap_or("")
            );
        }
    }
}
