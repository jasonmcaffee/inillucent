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
//! `crates/inillucent-compat/tests/selection.rs` attributes every source file
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
//! With none of them, it runs everything.
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
use inillucent_compat::selection::{self, Kind, Map, Row, Target};
use inillucent_compat::verdict::{self, Undetermined, Verdict};
use inillucent_compat::workspace_root;
use inillucent_scalar::json::node::Node;
use inillucent_scalar::json::{parse, render};

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
    /// Write the measured times back to the ledger.
    record: bool,
    /// Skip the build step, because the caller has just built.
    no_build: bool,
    /// Pass a filter through to each test binary.
    filter: Option<String>,
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
            jobs: other.jobs,
            test_threads: other.test_threads,
            list: other.list,
            list_tiers: other.list_tiers,
            strict: other.strict,
            record: other.record,
            no_build: other.no_build,
            filter: other.filter.clone(),
        }
    }
}

impl Default for Options {
    fn default() -> Options {
        Options {
            tiers: Vec::new(),
            targets: Vec::new(),
            changed: None,
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
            record: false,
            no_build: false,
            filter: None,
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
            "--all" => {
                options.tiers.clear();
                options.targets.clear();
                options.changed = None;
            }
            "--list" => options.list = true,
            "--list-tiers" => options.list_tiers = true,
            "--strict" => options.strict = true,
            "--record" => options.record = true,
            "--no-build" => options.no_build = true,
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
     Narrowing (with none of these, everything runs):\n  \
       --changed [<rev>]   run what the changes since <rev> (default HEAD) can break\n  \
       --tier <name>       run one tier; repeatable\n  \
       --target <label>    run one target as `package::name`; repeatable\n  \
       --filter <text>     pass a name filter to each test binary\n\
     \n\
     Execution:\n  \
       --jobs <n>          test binaries at once (default: the machine's cores)\n  \
       --test-threads <n>  threads inside each binary (default: 2)\n  \
       --no-build          do not build first\n\
     \n\
     Reporting:\n  \
       --list              print the selection and stop\n  \
       --list-tiers        print the tiers and stop\n  \
       --strict            fail when a selected suite's prerequisite is missing\n  \
       --record            write the measured times to tests/timings.toml\n"
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

/// Runs the whole thing.
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse_options(&arguments) {
        Ok(options) => options,
        Err(reason) => {
            eprintln!("{reason}");
            return ExitCode::FAILURE;
        }
    };
    match run(&options) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(reason) => {
            eprintln!("inillucent-testrun: {reason}");
            ExitCode::FAILURE
        }
    }
}

/// Selects, builds, runs and reports. Returns whether everything passed.
///
/// @param options - what the command line asked for
fn run(options: &Options) -> Result<bool, String> {
    let root = workspace_root();
    let map = Map::load(&root.join("tests/selection.toml"))?;

    if options.list_tiers {
        for tier in &map.tiers {
            println!(
                "{:<12} {}{}",
                tier.name,
                tier.purpose,
                if tier.exclusive { "  [runs alone]" } else { "" }
            );
        }
        return Ok(true);
    }

    let selected = choose(&root, &map, options)?;
    if selected.is_empty() {
        println!("nothing selected");
        return Ok(true);
    }

    if options.list {
        for row in &selected {
            println!(
                "{:<10} {:<44} covers {}",
                row.tier,
                row.target.label(),
                row.covers.iter().cloned().collect::<Vec<_>>().join(", ")
            );
        }
        println!("\n{} target(s)", selected.len());
        return Ok(true);
    }

    if !options.no_build {
        build(&root)?;
    }
    let built = locate(&root, &selected)?;
    let ledger = read_ledger(&root.join("tests/timings.toml"));
    // Kept by target so the retry pass can start one again. The workers take
    // their own clone, so nothing here is a second opinion about what was built.
    let executables: BTreeMap<Target, Built> = built
        .iter()
        .map(|one| (one.target.clone(), one.clone()))
        .collect();
    let ordered = schedule(built, &ledger);

    // Split off the targets whose tier asked for the machine to itself. They
    // go last, one at a time: see `selection::Tier::exclusive` for why a timing
    // guard cannot share a machine, and why widening its threshold instead
    // would only have made it stop guarding.
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
            .is_some_and(|row| exclusive_tiers.contains(row.tier.as_str()));
        if solo {
            alone.push(built);
        } else {
            shared.push(built);
        }
    }

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
    let mut outcomes = execute(shared, options);
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
        outcomes.extend(execute(alone, &solo));
    }
    let outcomes = settle_undetermined(outcomes, &executables, options);
    let wall = started.elapsed();

    report(&outcomes, wall, &map, options.strict);
    if options.record {
        write_ledger(&root.join("tests/timings.toml"), &ledger, &outcomes)?;
        println!("recorded {} timing(s)", outcomes.len());
    }

    // Undetermined counts as red, and deliberately so. The run is a gate, and
    // "the runner could not tell" is not evidence that anything passed. What
    // stops that being the old wrong red is the retry above: a target only stays
    // undetermined here when a second, solitary attempt could not read it either.
    let red = outcomes.iter().any(|outcome| !outcome.verdict.is_green());
    let hollow = options.strict && !missing_prerequisites(&outcomes, &map).is_empty();
    Ok(!red && !hollow)
}

/// Works out which rows to run.
///
/// @param root - the workspace root
/// @param map - the selection map
/// @param options - what the command line asked for
fn choose<'map>(root: &Path, map: &'map Map, options: &Options) -> Result<Vec<&'map Row>, String> {
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
        selection::select(map, &choice, &graph)
    } else {
        map.rows.iter().collect()
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

/// Builds every test binary, and the two programs the shell suites run.
///
/// The second half matters for parallelism. `cli.rs` and `semantics.rs` in this
/// crate, and `conformance.rs` in `inillucent-driver-capi`, each shell out to
/// `cargo build` from inside the test, because they drive a *program* rather
/// than a library. Run at once, they would each wait on cargo's lock on the
/// target directory. Building those programs here, before anything starts,
/// makes each of those in-test builds a no-op.
///
/// @param root - the workspace root
fn build(root: &Path) -> Result<(), String> {
    println!("building test targets");
    let status = Command::new(cargo())
        .current_dir(root)
        // `--lib --tests` and not a bare `--no-run`. A bare one also builds the
        // plain binaries, and one of those is *this program*: cargo cannot
        // replace an executable that is currently running, so the runner's own
        // build step failed with "Access is denied" trying to overwrite itself.
        // `--tests` still builds the test harness compiled from each binary's
        // sources, which is what `inillucent-bench` and `inillucent-shell` keep
        // their tests in; it just does not build the binaries themselves. The
        // two programs the shell suites actually execute are built below, by
        // name.
        .args(["test", "--workspace", "--no-run", "--lib", "--tests"])
        .status()
        .map_err(|error| format!("cannot run cargo: {error}"))?;
    if !status.success() {
        return Err("the build failed".to_string());
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
/// @param root - the workspace root
/// @param rows - the rows that were selected
fn locate(root: &Path, rows: &[&Row]) -> Result<Vec<Built>, String> {
    let output = Command::new(cargo())
        .current_dir(root)
        .args([
            "test",
            "--workspace",
            "--no-run",
            "--lib",
            "--tests",
            "--message-format=json",
        ])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("cannot run cargo: {error}"))?;
    if !output.status.success() {
        return Err("cargo could not list the built targets".to_string());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut found: BTreeMap<Target, (PathBuf, PathBuf)> = BTreeMap::new();
    for line in text.lines() {
        let Some(artifact) = read_artifact(line) else {
            continue;
        };
        found.insert(artifact.0, (artifact.1, artifact.2));
    }
    let mut built = Vec::new();
    let mut missing = Vec::new();
    for row in rows {
        match found.get(&row.target) {
            Some((executable, directory)) => built.push(Built {
                target: row.target.clone(),
                executable: executable.clone(),
                directory: directory.clone(),
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

/// Reads one `compiler-artifact` line, returning the target, its executable and
/// the directory cargo would run it from.
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
fn read_artifact(line: &str) -> Option<(Target, PathBuf, PathBuf)> {
    let parsed = parse::parse(line).ok()?;
    let node = parsed.node;
    if text_of(field(&node, "reason")?)? != "compiler-artifact" {
        return None;
    }
    if !matches!(field(field(&node, "profile")?, "test")?, Node::True) {
        return None;
    }
    let executable = text_of(field(&node, "executable")?)?;
    let manifest = text_of(field(&node, "manifest_path")?)?;
    let target = field(&node, "target")?;
    let name = text_of(field(target, "name")?)?;
    let kind = match field(target, "kind")? {
        Node::Array(items) => text_of(items.first()?)?,
        other => text_of(other)?,
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
    Some((
        Target {
            package,
            kind,
            name,
        },
        PathBuf::from(executable),
        directory,
    ))
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

/// Returns one member of a JSON object.
///
/// @param node - the object
/// @param name - the member's label
fn field<'tree>(node: &'tree Node, name: &str) -> Option<&'tree Node> {
    match node {
        Node::Object(members) => members
            .iter()
            .find(|(label, _)| render::unescape(label) == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

/// Returns a JSON string's content, with its escapes resolved.
///
/// The escapes are the point: every path in cargo's stream is a Windows path,
/// so every one of them arrives with doubled backslashes.
///
/// @param node - the string node
fn text_of(node: &Node) -> Option<String> {
    match node {
        Node::Text(_) | Node::TextJ(_) | Node::Text5(_) | Node::TextRaw(_) => {
            Some(render::unescape(node))
        }
        _ => None,
    }
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
fn execute(ordered: Vec<Built>, options: &Options) -> Vec<Outcome> {
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
        let _ = std::thread::Builder::new()
            .name(built.target.label())
            .spawn(move || {
                let outcome = run_one(&built, &threads, filter.as_deref(), strict);
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

/// Runs one test binary and reads its result.
///
/// @param built - the executable and where to run it
/// @param threads - what to pass as `--test-threads`
/// @param filter - a name filter, when one was asked for
/// @param strict - whether a suite that skips should panic rather than pass
fn run_one(built: &Built, threads: &str, filter: Option<&str>, strict: bool) -> Outcome {
    let mut command = Command::new(&built.executable);
    command
        .current_dir(&built.directory)
        // `cli.rs` and its two siblings read `CARGO` to find the cargo that is
        // driving them. Started from here rather than from cargo, they would
        // fall back to whatever `cargo` is on PATH, which on a machine with
        // several toolchains is not necessarily this one.
        .env("CARGO", cargo())
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
    if let Some(filter) = filter {
        command.arg(filter);
    }
    let started = Instant::now();
    let output = command.output();
    let elapsed = started.elapsed();
    match output {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            let summary = verdict::read_summary(&text).unwrap_or_default();
            Outcome {
                target: built.target.clone(),
                verdict: verdict::classify(&text, output.status.success()),
                elapsed,
                ran: summary.passed + summary.failed,
                output: text,
                status: match output.status.code() {
                    Some(code) => format!("exit status {code}"),
                    None => "killed by a signal".to_string(),
                },
                retry_of: None,
            }
        }
        Err(error) => Outcome {
            target: built.target.clone(),
            verdict: Verdict::Undetermined(Undetermined::NeverStarted),
            elapsed,
            ran: 0,
            output: format!("cannot start {}: {error}", built.executable.display()),
            status: "never started".to_string(),
            retry_of: None,
        },
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
/// @param outcomes - what the first pass produced
/// @param executables - the built binaries, by target
/// @param options - what the command line asked for
fn settle_undetermined(
    outcomes: Vec<Outcome>,
    executables: &BTreeMap<Target, Built>,
    options: &Options,
) -> Vec<Outcome> {
    let unread: Vec<(Target, Undetermined, String)> = outcomes
        .iter()
        .filter_map(|outcome| {
            outcome
                .verdict
                .undetermined()
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
        let mut again = run_one(built, &threads, options.filter.as_deref(), options.strict);
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
            let said = if row.requires.is_empty() {
                strict_reasons
            } else {
                row.requires.clone()
            };
            hollow.push((outcome, said));
        }
    }
    hollow
}

/// Prints the summary, and every failure in full.
///
/// @param outcomes - what ran
/// @param wall - how long the whole run took
/// @param map - the selection map
/// @param strict - whether a missing prerequisite is a failure
fn report(outcomes: &[Outcome], wall: Duration, map: &Map, strict: bool) {
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

    let hollow = missing_prerequisites(outcomes, map);
    if !hollow.is_empty() {
        println!(
            "\n{} suite(s) ran without a prerequisite and evidenced nothing{}:",
            hollow.len(),
            if strict {
                ""
            } else {
                " (--strict makes this a failure)"
            }
        );
        for (outcome, requires) in &hollow {
            // **"needs" only in front of a `requires` row.** Those are names of
            // things - `postgres`, `oracle`, `shell` - and read as a need. A
            // reason the suite printed is a whole sentence about this machine,
            // and putting "needs" in front of one produces "needs this platform
            // would not make a directory link" (task-1932, H10).
            let said = requires.join(", ");
            let lead = if said.split_whitespace().count() > 3 {
                ""
            } else {
                "needs "
            };
            println!("  {:<44} {lead}{}", outcome.target.label(), said);
        }
    }

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
        if strict && !hollow.is_empty() {
            println!(
                "\nnot ok - every test passed, and {} suite(s) evidenced nothing",
                hollow.len()
            );
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
    if !unread.is_empty() {
        println!("\nUNDETERMINED - a second attempt alone did not settle these either:");
        for outcome in &unread {
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
